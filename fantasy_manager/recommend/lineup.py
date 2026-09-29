"""Optimal starting lineup and start/sit recommendations.

The solver is exact: a dynamic program over players where the state is how many of each
starting-slot *type* are filled (slots of one type are interchangeable), so e.g. C2 LW2 RW2
D4 UTIL1 G2 has only 3*3*3*5*2*3 = 810 states. Eligibility: C/LW/RW/D/G by position, F takes
any forward, UTIL any skater, G only goalies; BN and IR are not starting slots and players
sitting in IR slots are not candidates.

Start/sit recommendations come from the slot diff between the current lineup and the optimal
one (re-arranged so as many starters as possible keep their current slot type): each change
is one chain "bench player enters slot T [, starter moves from T to T2 ...], a starter leaves",
so a player is only ever paired with a slot he can fill (a goalie only with a goalie slot).

Lineup lock: ``providers.fantrax.lineup_lock_for`` (the Fantrax Rules page, parsed once by
``FantraxProvider.lineup_lock``). Weekly-lock leagues get the "for this week's lineup (locks
Monday)" wording and a LINEUP_LOCK reason; daily leagues (ESPN, Fantrax set to daily) do not.

Daily-lineup leagues: when Daily Faceoff has named tonight's starter
(``Player.confirmed_start`` / ``start_source``, see providers.lines_enrich) and the goalie's
team plays today, today's game in his week projection uses a start probability of 1.0
(confirmed) / 0.8 (likely or expected) for the starter and 0.0 / 0.2 for the other goalie
instead of his season start share (CONFIRMED_START reason on the recs involving him).
"""
from __future__ import annotations

import re
from typing import Callable, Mapping, NamedTuple

from ..matching.normalize import normalize_team
from ..models import FantasyTeam, LeagueContext, Player, Reason, Recommendation
from ..valuation.adjust import Horizon
from ..valuation.replacement import eligible_slots
from ..valuation.valuate import PlayerValue
from .strength import apply_ranks, apply_strength

SLOT_ORDER = ("C", "LW", "RW", "F", "D", "UTIL", "G")
NON_STARTING = ("BN", "IR")
MIN_GAIN_ABS = 0.5
MIN_GAIN_REL = 0.08
STABILITY_BONUS = 1e-6       # tie-break toward current starters
UNAVAILABLE = ("out", "ir", "ltir", "suspended")
# Start probability for today's game from a Daily Faceoff report: (starter, other goalie).
START_PROB_CONFIRMED = (1.0, 0.0)
START_PROB_LIKELY = (0.8, 0.2)
_START_STRENGTH_RE = re.compile(r"\b(Confirmed|Likely|Expected)\b", re.I)


def starting_slots(roster_shape: Mapping[str, int]) -> list[str]:
    """Expanded starting slots in canonical order, e.g. ['C','C','LW','LW',...]."""
    order = list(SLOT_ORDER) + sorted(s for s in roster_shape if s not in SLOT_ORDER and s not in NON_STARTING)
    return [s for s in order for _ in range(int(roster_shape.get(s, 0)))]


def _value(pv: PlayerValue | None, horizon: Horizon) -> float:
    return pv.lineup_value(horizon) if pv is not None else 0.0


def _dp(players: list[Player], slots: list[str],
        weight: Callable[[Player, str], float]) -> tuple[float, dict[int, str]]:
    """Exact max-weight assignment of ``players`` to ``slots`` (weight per player and slot
    type) -> (score, {slot index: cid})."""
    types = list(dict.fromkeys(slots))
    caps = tuple(slots.count(t) for t in types)
    # dp: filled count per slot type -> (score, ((type index, cid), ...))
    dp: dict[tuple[int, ...], tuple[float, tuple[tuple[int, str], ...]]] = {tuple(0 for _ in types): (0.0, ())}
    for p in players:
        elig = set(eligible_slots(p.positions, types))
        idxs = [i for i, t in enumerate(types) if t in elig]
        if not idxs:
            continue
        ws = {i: weight(p, types[i]) for i in idxs}
        new = dict(dp)
        for state, (score, assign) in dp.items():
            for i in idxs:
                if state[i] >= caps[i]:
                    continue
                ns = state[:i] + (state[i] + 1,) + state[i + 1:]
                cand = score + ws[i]
                if ns not in new or cand > new[ns][0] + 1e-12:
                    new[ns] = (cand, assign + ((i, p.cid),))
        dp = new
    best_score, best_assign = max(dp.values(), key=lambda x: x[0])
    free: dict[str, list[int]] = {}
    for idx, s in enumerate(slots):
        free.setdefault(s, []).append(idx)
    out = {free[types[ti]].pop(0): cid for ti, cid in best_assign}
    return best_score, dict(sorted(out.items()))


def _solve(players: list[Player], values: Mapping[str, PlayerValue], slots: list[str], horizon: Horizon,
           current: set[str]) -> tuple[float, dict[int, str]]:
    """Exact max-value assignment of ``players`` to ``slots`` -> (score, {slot index: cid})."""
    vals = {p.cid: _value(values.get(p.cid), horizon) + (STABILITY_BONUS if p.cid in current else 0.0)
            for p in players}
    return _dp(players, slots, lambda p, t: vals[p.cid])


_PLACE_ALL = 1000.0     # >> number of players: placing everyone dominates keeping slot types


def _arrange(players: list[Player], slots: list[str], now: Mapping[str, str]) -> dict[int, str]:
    """Place every one of ``players`` (a feasible starting set) so that as many as possible
    stay in their current slot type ``now[cid]`` -> {slot index: cid}."""
    _, assign = _dp(players, slots, lambda p, t: _PLACE_ALL + (1.0 if now.get(p.cid) == t else 0.0))
    return assign


def _candidates(team: FantasyTeam) -> list[Player]:
    players = [s.player for s in team.slots if s.player is not None and s.slot != "IR"]
    return list({p.cid: p for p in players}.values())


def _starters(team: FantasyTeam) -> set[str]:
    return {s.player.cid for s in team.slots if s.player is not None and s.starting}


def _current_types(team: FantasyTeam) -> dict[str, str]:
    """{cid: starting slot type} for the current starters."""
    return {s.player.cid: s.slot for s in team.slots if s.player is not None and s.starting}


def optimal_lineup(team: FantasyTeam, values: Mapping[str, PlayerValue], roster_shape: Mapping[str, int],
                   horizon: Horizon = "week") -> tuple[dict[int, str], float]:
    """Best assignment {index into starting_slots(roster_shape): cid} and its total value."""
    slots = starting_slots(roster_shape)
    pool = _candidates(team)
    _, best = _solve(pool, values, slots, horizon, _starters(team))
    chosen = set(best.values())
    assign = _arrange([p for p in pool if p.cid in chosen], slots, _current_types(team))
    if set(assign.values()) != chosen:          # cannot happen (best is feasible); be safe
        assign = best
    total = sum(_value(values.get(c), horizon) for c in assign.values())
    return assign, total


def current_total(team: FantasyTeam, values: Mapping[str, PlayerValue], horizon: Horizon = "week") -> float:
    return sum(_value(values.get(s.player.cid), horizon) for s in team.slots
               if s.player is not None and s.starting)


def _week_reasons(p: Player, pv: PlayerValue | None, horizon: Horizon) -> list[Reason]:
    out: list[Reason] = []
    if pv is None:
        return out
    if pv.games_next7 is not None:
        out.append(Reason(code="GAMES_NEXT7", text=f"{p.name}: {pv.games_next7} games next 7 days"
                          + (f" ({pv.offnight_next7} off-night)" if pv.offnight_next7 else ""),
                          value=float(pv.games_next7)))
    if horizon == "week" and pv.proj_week is not None:
        out.append(Reason(code="PROJ_WEEK", text=f"{p.name}: {pv.proj_week:.1f} projected pts this week",
                          value=pv.proj_week))
    else:
        out.append(Reason(code="PROJ_WEEK" if horizon == "week" else "FPG",
                          text=f"{p.name}: {pv.lineup_value(horizon):.2f} FPG ({horizon})",
                          value=pv.lineup_value(horizon)))
    if p.status not in ("healthy", "unknown"):
        out.append(Reason(code="STATUS", text=f"{p.name} is {p.status}"
                          + (f" ({p.status_note})" if p.status_note else "")))
    return out


class SlotChange(NamedTuple):
    """One starting-slot type gets ``incoming`` (None: the slot is left empty) in place of
    ``outgoing`` (None: the slot was empty)."""
    slot: str
    incoming: str | None
    outgoing: str | None


def slot_changes(team: FantasyTeam, assign: Mapping[int, str], slots: list[str],
                 value: Callable[[str], float] | None = None) -> list[SlotChange]:
    """Per slot type, pair the players entering it with the players leaving it (or with its
    empty slots). Slots of one type are interchangeable, so a pair always shares a slot type
    both can fill: a goalie is only ever paired through a goalie slot. ``value`` orders the
    pairing (best incoming with worst outgoing first)."""
    val = value or (lambda c: 0.0)
    cur: dict[str, list[str]] = {}
    for rs in team.slots:
        if rs.player is not None and rs.starting:
            cur.setdefault(rs.slot, []).append(rs.player.cid)
    opt: dict[str, list[str]] = {}
    for i, cid in sorted(assign.items()):
        opt.setdefault(slots[i], []).append(cid)
    caps = {t: slots.count(t) for t in slots}
    out: list[SlotChange] = []
    for t in dict.fromkeys([*slots, *cur]):
        now, then = cur.get(t, []), opt.get(t, [])
        ins = sorted((c for c in then if c not in now), key=lambda c: (-val(c), c))
        outs = sorted((c for c in now if c not in then), key=lambda c: (val(c), c))
        partners: list[str | None] = [*outs, *([None] * max(0, caps.get(t, 0) - len(now)))]
        for k, c in enumerate(ins):
            out.append(SlotChange(t, c, partners[k] if k < len(partners) else None))
        for c in outs[len(ins):]:
            out.append(SlotChange(t, None, c))
    return out


def _chains(changes: list[SlotChange], starters: set[str]) -> list[list[SlotChange]]:
    """Link slot changes into chains: a player entering from the bench (or a slot left empty)
    starts one, a starter moving to another slot type continues it there, and a player
    leaving the lineup (or an empty slot) ends it."""
    by_in = {c.incoming: k for k, c in enumerate(changes) if c.incoming is not None}
    used: set[int] = set()
    chains: list[list[SlotChange]] = []

    def follow(k: int) -> list[SlotChange]:
        chain = [changes[k]]
        used.add(k)
        while (nxt := by_in.get(chain[-1].outgoing)) is not None and nxt not in used:
            chain.append(changes[nxt])
            used.add(nxt)
        return chain

    heads = [k for k, c in enumerate(changes) if c.incoming is None or c.incoming not in starters]
    for k in heads:
        if k not in used:
            chains.append(follow(k))
    for k in range(len(changes)):        # left over: starters only re-arranged (no gain)
        if k not in used:
            chains.append(follow(k))
    return chains


def _leaving(p: Player) -> str:
    return f"{p.name} ({p.status})" if p.status in UNAVAILABLE else p.name


def _chain_title(chain: list[SlotChange], by_cid: Mapping[str, Player]) -> str:
    """"Start A over B (suspended)", "Start A at G (empty slot)", or for a chain
    "Start A at UTIL for B; move B to C over D (ir)"."""
    first = chain[0]
    if len(chain) == 1:
        if first.incoming is None:
            return f"Bench {_leaving(by_cid[first.outgoing])} (leave {first.slot} empty)"
        if first.outgoing is None:
            return f"Start {by_cid[first.incoming].name} at {first.slot} (empty slot)"
        return f"Start {by_cid[first.incoming].name} over {_leaving(by_cid[first.outgoing])}"
    parts = []
    for k, c in enumerate(chain):
        if c.incoming is None:
            head = f"leave {c.slot} empty"
        else:
            verb = "Start" if k == 0 else "move"
            head = f"{verb} {by_cid[c.incoming].name} {'at' if k == 0 else 'to'} {c.slot}"
        if c.outgoing is None:
            tail = " (empty slot)"
        elif k == len(chain) - 1:
            tail = f" over {_leaving(by_cid[c.outgoing])}"
        else:
            tail = f" for {by_cid[c.outgoing].name}"
        parts.append(head + tail)
    title = "; ".join(parts)
    return title[0].upper() + title[1:]


def confirmed_start_probability(p: Player) -> float | None:
    """Today's start probability from ``confirmed_start`` (None when unknown): 1.0 / 0.0 for
    a confirmed report, 0.8 / 0.2 for likely / expected (read from ``start_source``)."""
    if not p.is_goalie or p.confirmed_start is None:
        return None
    m = _START_STRENGTH_RE.search(p.start_source or "")
    starter, other = START_PROB_LIKELY if m and m.group(1).lower() != "confirmed" else START_PROB_CONFIRMED
    return starter if p.confirmed_start else other


def apply_confirmed_starts(ctx: LeagueContext, values: Mapping[str, PlayerValue], horizon: Horizon = "week"
                           ) -> tuple[Mapping[str, PlayerValue], dict[str, tuple[float, float]]]:
    """Daily-lineup leagues (ESPN, Fantrax set to daily), week horizon: re-project my goalies
    whose start tonight is known.
    Today's game counts ``prob`` starts instead of the start share, i.e. ``proj_week +
    proj_week / (games * share) * (prob - share)``. Returns (values, {cid: (prob, share)})."""
    if horizon != "week" or _lock(ctx) != "daily":
        return values, {}
    try:
        team = ctx.my_team
    except LookupError:
        return values, {}
    out: dict[str, PlayerValue] | None = None
    used: dict[str, tuple[float, float]] = {}
    for p in team.players:
        prob = confirmed_start_probability(p)
        pv = values.get(p.cid)
        if prob is None or pv is None or pv.proj_week is None or not pv.games_next7:
            continue
        if ctx.as_of not in (ctx.schedule.get(normalize_team(p.team) or "") or ()):
            continue
        share = pv.start_share if pv.start_share is not None else 1.0
        if share <= 0:
            continue
        per_start = pv.proj_week / (pv.games_next7 * share)
        new = max(0.0, pv.proj_week + per_start * (prob - share))
        if out is None:
            out = dict(values)
        out[p.cid] = pv.model_copy(update={"proj_week": new})
        used[p.cid] = (prob, share)
    return (out if out is not None else values), used


def _start_reason(p: Player, prob: float, share: float) -> Reason:
    return Reason(code="CONFIRMED_START",
                  text=f"{p.name}: start probability today {prob:.0%} (season share {share:.0%}); "
                       f"{p.start_source or 'Daily Faceoff'}", value=prob, baseline=share)


def recommend_lineup(ctx: LeagueContext, values: Mapping[str, PlayerValue],
                     horizon: Horizon = "week") -> list[Recommendation]:
    """Start/sit changes that beat the current lineup, plus injured starters and healthy IR
    players. One rec per chain of slot changes between the current and the optimal lineup;
    its gain is the week (or FPG) delta of that chain, and the gains of all chains add up to
    optimal - current."""
    team = ctx.my_team
    values, starts = apply_confirmed_starts(ctx, values, horizon)
    recs: list[Recommendation] = []
    slots = starting_slots(ctx.roster_shape)
    by_cid = {p.cid: p for p in team.players}
    starters = _starters(team)
    assign, _ = optimal_lineup(team, values, ctx.roster_shape, horizon)
    after = set(assign.values())
    vc = lambda c: _value(values.get(c), horizon)  # noqa: E731
    for chain in _chains(slot_changes(team, assign, slots, vc), starters):
        head, tail = chain[0].incoming, chain[-1].outgoing
        pin = by_cid[head] if head is not None and head not in starters else None
        pout = by_cid[tail] if tail is not None and tail not in after else None
        moved = [by_cid[c.incoming] for c in chain if c.incoming is not None and c.incoming in starters]
        gain = (vc(pin.cid) if pin else 0.0) - (vc(pout.cid) if pout else 0.0)
        v_out = vc(pout.cid) if pout else 0.0
        threshold = max(MIN_GAIN_ABS, MIN_GAIN_REL * abs(v_out))
        if gain <= threshold:
            continue
        reasons = [Reason(code="LINEUP_GAIN", text=f"+{gain:.2f} ({horizon}) from this change",
                          value=gain, baseline=threshold)]
        for x in (pin, pout):
            if x is not None:
                reasons += _week_reasons(x, values.get(x.cid), horizon)
        for c in chain:
            if c.incoming in starters:
                reasons.append(Reason(code="LINEUP_MOVE", text=f"{by_cid[c.incoming].name} moves to {c.slot}"
                                      + (f" (replacing {by_cid[c.outgoing].name})" if c.outgoing else "")))
        units, days = _gain_units([x for x in (pin, pout) if x is not None] or moved, values, horizon)
        recs.append(Recommendation(kind="lineup", score=gain, title=_chain_title(chain, by_cid),
                                   add=[pin] if pin else [], drop=[pout] if pout else [], subjects=moved,
                                   reasons=reasons, predicted_gain=float(gain), gain_units=units,
                                   horizon_days=days))

    for s in team.slots:
        p = s.player
        if p is None:
            continue
        pv = values.get(p.cid)
        if s.starting and p.status in UNAVAILABLE and p.cid not in {r.drop[0].cid for r in recs if r.drop}:
            recs.append(Recommendation(
                kind="lineup", score=pv.fpg if pv else 0.0,
                title=f"Bench {p.name}: {p.status} but in a starting slot ({s.slot})", drop=[p],
                reasons=[Reason(code="STATUS", text=f"{p.name} is {p.status}"
                                + (f" ({p.status_note})" if p.status_note else ""))]
                + _week_reasons(p, pv, horizon)[:1]))
        elif s.slot == "IR" and p.status in ("healthy", "dtd"):
            recs.append(Recommendation(
                kind="lineup", score=pv.fpg_season if pv else 0.0,
                title=f"Activate {p.name} from IR ({p.status})", add=[p],
                predicted_gain=_value(pv, horizon) if pv else None,
                gain_units=_gain_units([p], values, horizon)[0] if pv else None,
                horizon_days=_gain_units([p], values, horizon)[1] if pv else None,
                reasons=[Reason(code="STATUS", text=f"{p.name} is {p.status} but sits in an IR slot"
                                + (f" ({p.status_note})" if p.status_note else ""))]
                + _week_reasons(p, pv, horizon)[:1]))
    if starts:
        by_cid_all = {p.cid: p for p in team.players}
        for r in recs:
            for cid in dict.fromkeys(x.cid for x in (*r.add, *r.drop, *r.subjects)):
                if cid in starts:
                    r.reasons.append(_start_reason(by_cid_all[cid], *starts[cid]))
    if _lock(ctx) == "weekly":
        recs = [_weekly_lock(r) for r in recs]
    recs.sort(key=lambda r: r.score, reverse=True)
    return apply_ranks(apply_strength(recs))


def _gain_units(players: list[Player], values: Mapping[str, PlayerValue], horizon: Horizon
                ) -> tuple[str, int | None]:
    """Units of a lineup gain: week points when every player involved has a projected week
    (schedule loaded), else FPG (the week's per-game value, or rest of season)."""
    if horizon == "week":
        pvs = [values.get(p.cid) for p in players]
        if pvs and all(pv is not None and pv.proj_week is not None for pv in pvs):
            return "week_pts", 7
        return "season_fpg", 7
    return "season_fpg", None


WEEKLY_LOCK_TEXT = "for this week's lineup (locks Monday)"


def _lock(ctx: LeagueContext) -> str:
    from ..providers.fantrax import lineup_lock_for

    return lineup_lock_for(ctx)


def _weekly_lock(r: Recommendation) -> Recommendation:
    """Weekly-lock leagues (Fantrax "Lineup changes are executed: Weekly"): the lineup is set once
    per scoring period, locking Monday."""
    return r.model_copy(update={
        "title": f"{r.title} {WEEKLY_LOCK_TEXT}",
        "reasons": list(r.reasons) + [Reason(code="LINEUP_LOCK",
                                             text="Fantrax lineup changes are executed weekly (Rules page): "
                                                  "the lineup locks Monday for the whole scoring period, "
                                                  "so this uses the week horizon")],
    })
