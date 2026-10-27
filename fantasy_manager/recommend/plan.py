"""The weekly action plan (``fm plan``, the dashboard's /plan page): one ordered, day-by-day plan for
the rest of the current matchup period in daily-lineup leagues.

Sections:

* **Header.** Score so far, projected totals and my win probability with quick lineup (the
  platform's Quick Lineup / auto-set pressed each day: the best lineup each day with no moves, the
  baseline everything is compared against) and with the full plan. ``win_as_set`` / ``proj_as_set``
  hold that baseline (so ``lineup_gain`` is 0: the plan takes no credit for setting lineups);
  ``win_frozen`` / ``proj_frozen`` keep the lineup left exactly as set all week, for reference.
  Win probabilities reuse ``analysis.matchup`` (opponent's best daily lineups, the same per-game
  SDs and Monte Carlo draws, a fixed seed so the numbers are comparable).
* **Today.** IR swaps / activations and today's start/sit changes (``recommend_lineup``), injury
  cuts and IR moves (``recommend_injuries`` / hold-or-cut verdicts; hold verdicts go to "Watch"),
  goalie starts (``analysis.goalie_plan``) and any move scheduled for today.
* **Add scheduling.** Which pickups, dropping whom, on which day, within the moves left. Candidate
  (add, drop) pairs are the this-matchup pickups of ``waivers.recommend_waivers(stream=True)``, so
  every drop already passed the waiver rules (roster legality, protected dynasty players, the
  season / dynasty tolerance, churn guard, unsigned free agents). On top of them: an injured player
  is never the drop here (his hold-or-cut verdict decides that, shown in Today), and a streamer the
  plan adds may later be dropped for another one ("add X Tue, drop him Thu for Y"). A move made on
  day d counts from its *effective* day: the same day on ESPN while the add's game has not started,
  on Fantrax the same day when made before the day's first NHL game (the league's transaction log
  shows each claim's effective period: morning adds count that day, adds after the games the next),
  the day after ``waiver_until`` for a claim. Its value is what it adds to the best lineup set each remaining day:
  bench games count 0. A beam search (``BEAM`` states, ``EXPAND`` best moves each) picks the
  sequence; each kept move must add at least ``MIN_MOVE_PTS[stance]`` matchup points on its own, and
  in the protect stance one move is held back for an injury.
* **Each later day.** One line: the moves that day (run quick lineup again after an add), the
  starts where the best lineup differs from what quick lineup would set (``_quick_choice``: players
  with a game ranked by their season value, as a platform does, vs our day values with roles,
  matchups and goalie start odds; only when it is worth ``QUICK_EDGE_MIN`` points), goalie starts,
  and a pre-game flag when a goalie of mine projected to start is not a confirmed starter.
  Otherwise "Quick lineup is fine".

All point numbers are real matchup points over the remaining days (no weekly extrapolation).
``build_week_plan`` is shared by the CLI, the dashboard and ``archive_plan`` (``data/archive/
plan-<league>-<date>.json``: predicted plan total / as-set total / win probabilities and the steps,
for grading against the final score).
"""
from __future__ import annotations

import json
from datetime import date, datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Iterable, Mapping

from pydantic import BaseModel, Field

from ..models import FantasyTeam, LeagueContext, Player, Recommendation
from .base import moves_left, shares_slot
from .headline import market_suffix
from .lineup import UNAVAILABLE, _dp, _lock, _starters, day_value, plan_days, started, starting_slots

MIN_MOVE_PTS = {"chase": 0.5, "even": 1.0, "protect": 2.0}   # matchup points a scheduled move must add
UNLIMITED_MOVES_CAP = 4        # moves the plan schedules when the league has no limit
CANDIDATE_LIMIT = 40           # this-matchup pickups considered (waivers.recommend_waivers limit)
BEAM = 3
EXPAND = 6
WIN_SEED = 7                   # fixed seed: the three win probabilities use the same draws
LOCK_BONUS = 1e6
PREGAME_CONFIRMED = 0.9        # a goalie start probability below this is worth a 5 pm check
QUICK_EDGE_MIN = 0.5           # day points the best lineup must beat quick lineup by to call out a start


class PlanMove(BaseModel):
    """One scheduled acquisition."""
    day: date                          # when to make it
    effective: date                    # first day it counts
    add: Player
    drop: Player | None = None
    to_ir: Player | None = None
    gain_pts: float = 0.0              # matchup points it adds (with the moves before it applied)
    win_delta: float | None = None
    starts: list[date] = Field(default_factory=list)   # days the add is in the best lineup
    headline: str = ""
    market: str | None = None          # "market agrees" / "model disagrees with market" / ...
    claim: bool = False
    reasons: list[str] = Field(default_factory=list)
    chain: int | None = None           # moves sharing a rented player (rental then swap) share a number
    role: str | None = None            # "rental" (a later step drops him) / "swap" (drops an earlier pickup)
    note: str = ""                     # the chain story: "Rent X Mon-Sat..." / "Then swap X for Y Sun: ..."
    timing: str = ""                   # when to make it: "Make this move the evening before (...)"
    status: str = ""                   # vs the last plan shown: "new" / "kept" (stability) / "" unchanged


class PlanSince(BaseModel):
    """What changed since the last plan shown (``plan_memory.compare``)."""
    at: str = ""                       # when that plan was made, local: "Mon 2:43 PM"
    done: list[str] = Field(default_factory=list)       # "Done: added X, dropped Y (Mon)"
    changed: list[str] = Field(default_factory=list)    # removed steps, each with its reason
    new: list[str] = Field(default_factory=list)        # steps that were not in it
    kept: list[str] = Field(default_factory=list)       # earlier steps kept for stability
    roster: list[str] = Field(default_factory=list)     # roster changes outside the plan
    long_term: list[str] = Field(default_factory=list)  # dropped keeper-type steps worth a later move

    @property
    def empty(self) -> bool:
        return not (self.done or self.changed or self.new or self.kept or self.roster or self.long_term)


class PlanAction(BaseModel):
    """A today item taken from an existing recommendation (lineup / injury)."""
    kind: str
    title: str
    headline: str | None = None
    gain: float | None = None
    gain_units: str | None = None


class PlanDayLine(BaseModel):
    day: date
    moves: list[PlanMove] = Field(default_factory=list)
    ins: list[str] = Field(default_factory=list)       # starts quick lineup would miss: "Saros (over Blackwood, ...)"
    outs: list[str] = Field(default_factory=list)      # players quick lineup would start that should sit
    goalies: list[str] = Field(default_factory=list)   # "Saros 62% vs BOS"
    pregame: bool = False
    quick_ok: bool = True                              # quick lineup sets this day's best lineup
    pts_plan: float = 0.0
    pts_as_set: float = 0.0                            # quick lineup with today's roster (no moves)
    pts_frozen: float = 0.0                            # lineup left exactly as set
    note: str = ""                                     # why the plan is below quick lineup that day
    text: str = ""


class WeekPlan(BaseModel):
    league: str
    my_team: str
    opponent: str | None = None
    start: date | None = None
    end: date | None = None
    days_left: int = 0
    my_so_far: float = 0.0
    their_so_far: float = 0.0
    their_remaining: float = 0.0
    stance: str | None = None
    moves_left: int | None = None
    moves_planned: int = 0
    baseline: str = "quick_lineup"      # what "as set" means: quick lineup pressed each day
    proj_as_set: float = 0.0            # remaining points with quick lineup each day, no moves (= proj_lineup)
    proj_lineup: float = 0.0            # best lineup each day, no moves
    proj_plan: float = 0.0              # best lineup each day with the scheduled moves
    proj_frozen: float = 0.0            # lineup left exactly as set all week (reference only)
    win_as_set: float | None = None     # = win_lineup (the quick-lineup baseline)
    win_lineup: float | None = None
    win_plan: float | None = None
    win_frozen: float | None = None     # lineup left exactly as set all week (reference only)
    today: list[PlanAction] = Field(default_factory=list)
    moves: list[PlanMove] = Field(default_factory=list)
    days: list[PlanDayLine] = Field(default_factory=list)
    watch: list[str] = Field(default_factory=list)
    claim_watch: list[str] = Field(default_factory=list)   # claims on waivers not scheduled (see the claim list)
    net_week: list[str] = Field(default_factory=list)      # who is gone / stays, long-term change, points
    since: PlanSince | None = None                         # vs the last plan shown (plan memory)
    notes: list[str] = Field(default_factory=list)
    available: bool = True

    @property
    def lineup_gain(self) -> float:
        return self.proj_lineup - self.proj_as_set

    @property
    def moves_gain(self) -> float:
        return self.proj_plan - self.proj_lineup

    def to_json(self) -> dict[str, Any]:
        d = self.model_dump(mode="json")
        d.update(lineup_gain=round(self.lineup_gain, 2), moves_gain=round(self.moves_gain, 2),
                 my_projected_total=round(self.my_so_far + self.proj_plan, 2),
                 their_projected_total=round(self.their_so_far + self.their_remaining, 2))
        return d


# --------------------------------------------------------------------------- timeline evaluation

class _Tx(BaseModel):
    day: date
    effective: date
    add: Player
    drop: Player | None = None
    to_ir: Player | None = None
    rec: Recommendation | None = None


class _Timeline:
    """Day totals of my best daily lineup for a roster that changes with scheduled moves."""

    def __init__(self, ctx: LeagueContext, team: FantasyTeam, values: Mapping[str, Any], days: list[date],
                 now: datetime):
        self.ctx, self.values, self.days = ctx, values, days
        self.slots = starting_slots(ctx.roster_shape)
        self.base = [s.player for s in team.slots if s.player is not None and s.slot != "IR"]
        self.starters = _starters(team)
        today = ctx.as_of
        self.locked = frozenset(p.cid for p in self.base if started(ctx, p, today, now))
        self._vals: dict[tuple[str, date], float] = {}
        self._memo: dict[tuple[date, frozenset[str]], tuple[float, frozenset[str]]] = {}

    def value(self, p: Player, d: date) -> float:
        key = (p.cid, d)
        if key not in self._vals:
            self._vals[key] = day_value(self.ctx, p, self.values.get(p.cid), d)
        return self._vals[key]

    def roster(self, d: date, txs: Iterable[_Tx]) -> list[Player]:
        """Active roster on ``d``: a move's add, drop and IR move all count from its effective day."""
        out = {p.cid: p for p in self.base}
        for t in txs:
            if t.effective <= d:
                out[t.add.cid] = t.add
                for gone in (t.drop, t.to_ir):
                    if gone is not None:
                        out.pop(gone.cid, None)
        return list(out.values())

    def day(self, d: date, players: list[Player]) -> tuple[float, frozenset[str]]:
        key = (d, frozenset(p.cid for p in players))
        hit = self._memo.get(key)
        if hit is not None:
            return hit
        today = d == self.ctx.as_of
        pool = [p for p in players if (self.value(p, d) > 0 or (today and p.cid in self.locked))
                and not (today and p.cid in self.locked and p.cid not in self.starters)]
        vals = {p.cid: self.value(p, d) for p in pool}
        weight = {c: v + (LOCK_BONUS if today and c in self.locked else 0.0) for c, v in vals.items()}
        _, assign = _dp(pool, self.slots, lambda p, t: weight[p.cid])
        chosen = frozenset(assign.values())
        res = (sum(vals[c] for c in chosen), chosen)
        self._memo[key] = res
        return res

    def as_set(self, d: date) -> float:
        by = {p.cid: p for p in self.base}
        return sum(self.value(by[c], d) for c in self.starters if c in by)

    def total(self, txs: list[_Tx]) -> float:
        return sum(self.day(d, self.roster(d, txs))[0] for d in self.days)

    def per_day(self, txs: list[_Tx]) -> list[tuple[float, frozenset[str]]]:
        return [self.day(d, self.roster(d, txs)) for d in self.days]


def _upto(txs: list[_Tx], i: int) -> list[_Tx]:
    """Moves 0..i, with players they add that a later move drops (a re-stream) already gone from that
    later move's effective day, so step i is credited only with the nights it has in the final plan
    and the steps still add up to the plan total. A removal is a _Tx adding and dropping the same
    player (``_Timeline.roster`` applies the add, then the drop)."""
    head = txs[:i + 1]
    added = {t.add.cid for t in head}
    gone = [t for t in txs[i + 1:] if t.drop is not None and t.drop.cid in added]
    return head + [_Tx(day=t.day, effective=t.effective, add=t.drop, drop=t.drop) for t in gone]


# --------------------------------------------------------------------------- candidates and search

def first_game_started(ctx: LeagueContext, day: date, now: datetime) -> bool:
    """The day's first NHL game has started by ``now`` (Fantrax's daily period lock)."""
    starts = [s[day] for s in ctx.game_starts.values() if day in s]
    return bool(starts) and now >= min(starts)


def _earliest(ctx: LeagueContext, p: Player, now: datetime) -> date:
    """First day a pickup made today can count (see the module docstring)."""
    today = ctx.as_of
    if p.waiver_until is not None and p.waiver_until >= today:
        return p.waiver_until + timedelta(days=1)
    late = first_game_started(ctx, today, now) if ctx.provider == "fantrax" else started(ctx, p, today, now)
    return today + timedelta(days=1) if late else today


def _local_zone() -> Any:
    """Zone for showing game times: FM_TZ / TZ when zoneinfo knows it, else None, which makes
    ``astimezone`` use the OS zone with its DST rules (Windows ships no tzdata)."""
    import os
    from zoneinfo import ZoneInfo

    for var in ("FM_TZ", "TZ"):
        name = (os.environ.get(var) or "").strip()
        if name:
            try:
                return ZoneInfo(name)
            except Exception:  # noqa: BLE001 - unknown or missing zone data: fall back to the OS zone
                continue
    return None


def _clock(t: datetime) -> str:
    """``t`` in local time, e.g. "11:00 AM" (the zone's offset on that instant, so DST days are right)."""
    loc = (t if t.tzinfo else t.replace(tzinfo=timezone.utc)).astimezone(_local_zone())
    return f"{loc.hour % 12 or 12}:{loc.minute:02d} {'AM' if loc.hour < 12 else 'PM'}"


def _game_time(ctx: LeagueContext, p: Player, d: date) -> datetime | None:
    from ..matching.normalize import normalize_team

    return (ctx.game_starts.get(normalize_team(p.team) or "") or {}).get(d)


def _first_game(ctx: LeagueContext, d: date) -> datetime | None:
    starts = [s[d] for s in ctx.game_starts.values() if d in s]
    return min(starts) if starts else None


def _timing(ctx: LeagueContext, mv: PlanMove) -> tuple[str, date]:
    """(when to make ``mv`` so it counts from its effective day and the dropped player keeps his
    points, the day to make it). A move made after a day's first game counts from the next day
    (Fantrax's daily lock; on ESPN the drop is gone too), so when the dropped player plays the day
    before the effective day, the move is made that evening, after his game has started."""
    eff, today = mv.effective, ctx.as_of
    if mv.claim and mv.add.waiver_until is not None:
        return (f"Place the claim now: it clears {_wd(mv.add.waiver_until)} and counts from {_wd(eff)}",
                mv.day)
    prev = eff - timedelta(days=1)
    if mv.drop is not None and prev >= today and (g := _game_time(ctx, mv.drop, prev)) is not None:
        return (f"Make this move the evening before (after {mv.drop.name}'s game starts {_wd(prev)} at "
                f"{_clock(g)} your time), so he keeps those points and {mv.add.name} counts {_wd(eff)}", prev)
    first = _first_game(ctx, eff)
    when = "today" if eff == today else _wd(eff)
    if first is None:
        return f"Make this move by {when}", mv.day
    return f"Make this move before {when}'s first game ({_clock(first)} your time)", mv.day


def _move_day(ctx: LeagueContext, p: Player, effective: date) -> date:
    """When to make a move that should count from ``effective``: claims are placed now, free-agent
    adds that morning (Fantrax: before the day's first game; ESPN: before his game)."""
    today = ctx.as_of
    if p.waiver_until is not None and p.waiver_until >= today:
        return today
    return max(today, effective)


def _candidates(ctx: LeagueContext, values: Mapping[str, Any], dynasty_values: Mapping[str, Any] | None
                ) -> tuple[list[tuple[Player, Player | None, Player | None, Recommendation]], list[str]]:
    """(add, drop, to_ir, rec) pairs from the this-matchup pickups; injured players are never the drop."""
    from .waivers import recommend_waivers

    notes: list[str] = []
    hurt = {p.cid for p in ctx.my_team.players if p.status in UNAVAILABLE}
    try:
        recs = recommend_waivers(ctx, dict(values), limit=CANDIDATE_LIMIT, horizon="week",
                                 dynasty_values=dynasty_values, stream=True, exclude_drops=hurt)
    except Exception as e:  # noqa: BLE001 - the plan still sets lineups without pickups
        return [], [f"Pickup candidates unavailable: {e}"]
    out = []
    for r in recs:
        if not r.add:
            continue
        drop = r.drop[0] if r.drop else None
        to_ir = r.subjects[0] if (drop is None and r.subjects) else None
        out.append((r.add[0], drop, to_ir, r))
    return out, notes


def _best_day(tl: _Timeline, txs: list[_Tx], add: Player, drop: Player | None, to_ir: Player | None,
              earliest: date) -> tuple[float, date] | None:
    """Best day to make a move on top of ``txs``: (gain, effective day). Making it later keeps the
    dropped player's earlier games, so the best day maximises the suffix sum of daily gains."""
    deltas = []
    for d in tl.days:
        if d < earliest:
            deltas.append(None)
            continue
        before = tl.roster(d, txs)
        after = [p for p in before if p.cid not in {x.cid for x in (drop, to_ir) if x is not None}] + [add]
        deltas.append(tl.day(d, after)[0] - tl.day(d, before)[0])
    best: tuple[float, date] | None = None
    run = 0.0
    for i in range(len(tl.days) - 1, -1, -1):
        if deltas[i] is None:
            break
        run += deltas[i]
        if best is None or run > best[0] + 1e-9:
            best = (run, tl.days[i])
    return best


def _long_term(ctx: LeagueContext, values: Mapping[str, Any], dyn: Mapping[str, float] | None,
               add: Player, drop: Player) -> tuple[bool, str]:
    """(within tolerance, reason text) of swapping ``drop`` for ``add`` long term: dynasty value in
    dynasty leagues, else rest-of-season FPG, with the waiver engine's stream tolerance
    (``waivers.STREAM_DYNASTY_TOLERANCE`` for the league's mode). Always against the player this
    step actually drops (a re-stream drops an earlier pickup, not the waiver rec's drop)."""
    from .waivers import STREAM_DYNASTY_TOLERANCE

    tol = STREAM_DYNASTY_TOLERANCE.get(getattr(ctx, "dynasty_mode", None) or "contend", 0.0)
    if dyn is not None:
        a, d, unit, lead = dyn.get(add.cid, 0.0), dyn.get(drop.cid, 0.0), "dynasty value", "Long term"
        fmt = "{:.1f}"
    else:
        a = float(getattr(values.get(add.cid), "fpg_season", 0.0) or 0.0)
        d = float(getattr(values.get(drop.cid), "fpg_season", 0.0) or 0.0)
        unit, lead, fmt = "FPG", "Rest of season", "{:.2f}"
    text = f"{lead}: {add.name} {fmt.format(a)} vs {drop.name} {fmt.format(d)} {unit} ("
    if a >= d:
        text += f"an upgrade, +{fmt.format(a - d)})"
    else:
        text += f"costs {fmt.format(d - a)}, {(d - a) / d:.0%} of {drop.name}'s)"
    return a >= (1.0 - tol) * d, text


def _search(ctx: LeagueContext, tl: _Timeline, pairs: list, max_moves: int, min_pts: float,
            now: datetime, values: Mapping[str, Any] | None = None,
            dyn: Mapping[str, float] | None = None) -> list[_Tx]:
    """Beam search over move sequences (module docstring). A re-stream (dropping a pickup the plan
    made earlier) must also stay within the long-term tolerance against that pickup (``_long_term``)."""
    states: list[tuple[float, list[_Tx]]] = [(tl.total([]), [])]
    best = states[0]
    for _ in range(max_moves):
        nxt: list[tuple[float, list[_Tx]]] = []
        for total, txs in states:
            added = {t.add.cid for t in txs}
            gone = {x.cid for t in txs for x in (t.drop, t.to_ir) if x is not None}
            options = []
            for add, drop, to_ir, rec in pairs:
                if add.cid in added or (drop is not None and drop.cid in gone) or \
                        (to_ir is not None and to_ir.cid in gone):
                    continue
                if drop is None and any(t.drop is None and t.to_ir is None for t in txs) and to_ir is None:
                    continue                  # one open roster spot is used once
                hit = _best_day(tl, txs, add, drop, to_ir, _earliest(ctx, add, now))
                if hit is not None and hit[0] >= min_pts:
                    options.append((hit[0], _Tx(day=_move_day(ctx, add, hit[1]), effective=hit[1], add=add,
                                                drop=drop, to_ir=to_ir, rec=rec)))
            # re-stream: drop a streamer this plan added for another pickup later in the week
            for prev in txs:
                if prev.add.cid in gone:
                    continue
                for add, _, _, rec in pairs:
                    if add.cid in added or not shares_slot(add, prev.add):
                        continue
                    if values is not None and not _long_term(ctx, values, dyn, add, prev.add)[0]:
                        continue
                    earliest = max(_earliest(ctx, add, now), prev.effective + timedelta(days=1))
                    hit = _best_day(tl, txs, add, prev.add, None, earliest)
                    if hit is not None and hit[0] >= min_pts:
                        options.append((hit[0], _Tx(day=_move_day(ctx, add, hit[1]), effective=hit[1], add=add,
                                                    drop=prev.add, rec=rec)))
            options.sort(key=lambda o: -o[0])
            for g, tx in options[:EXPAND]:
                nxt.append((total + g, txs + [tx]))
        if not nxt:
            break
        nxt.sort(key=lambda s: -s[0])
        states = nxt[:BEAM]
        if states[0][0] > best[0] + 1e-9:
            best = states[0]
    return _prune(tl, best[1], min_pts)


def _prune(tl: _Timeline, txs: list[_Tx], min_pts: float) -> list[_Tx]:
    """Drop moves whose own contribution (plan without it vs with it) is below ``min_pts``."""
    txs = list(txs)
    changed = True
    while changed and txs:
        changed = False
        full = tl.total(txs)
        for t in txs:
            rest = [x for x in txs if x is not t and not (x.drop is not None and x.drop.cid == t.add.cid)]
            if full - tl.total(rest) < min_pts:
                txs = rest
                changed = True
                break
    return sorted(txs, key=lambda t: (t.day, t.effective))


# --------------------------------------------------------------------------- win probability

def _win(tl: _Timeline, txs: list[_Tx], m: Any, as_set: bool = False) -> float | None:
    from ..analysis.matchup import game_sd, win_probability

    if m is None or m.opponent_team is None:
        return None
    pts: dict[str, list[float]] = {}
    goalie: dict[str, bool] = {}
    for d in tl.days:
        if as_set:
            for p in tl.base:
                if p.cid in tl.starters and (v := tl.value(p, d)) > 0:
                    pts.setdefault(p.cid, []).append(v)
                    goalie[p.cid] = p.is_goalie
            continue
        players = tl.roster(d, txs)
        by = {p.cid: p for p in players}
        for c in tl.day(d, players)[1]:
            v = tl.value(by[c], d)
            if v > 0:
                pts.setdefault(c, []).append(v)
                goalie[c] = by[c].is_goalie
    mine = []
    for c, xs in pts.items():
        mean = sum(xs)
        mine.append((mean, (len(xs) ** 0.5) * game_sd(mean / len(xs), goalie[c])))
    theirs = [(x.proj_remaining, x.sd) for x in m.their_players]
    return win_probability(m.my_points_so_far, m.their_points_so_far, mine, theirs, draws=m.draws, seed=WIN_SEED)


# --------------------------------------------------------------------------- text helpers

def _wd(d: date) -> str:
    return f"{d:%a}"


def _move_headline(tl: _Timeline, mv: PlanMove, lost: list[date] | None = None) -> str:
    """``lost``: nights the dropped player would have started from the move on (None: count his games)."""
    days = ", ".join(_wd(d) for d in mv.starts) or "no nights"
    head = f"{mv.add.name} starts {len(mv.starts)} night{'s' if len(mv.starts) != 1 else ''} for you ({days})"
    if mv.drop is not None:
        games = sum(1 for d in tl.days if d >= mv.effective and tl.value(mv.drop, d) > 0)
        if not games:
            head += f"; {mv.drop.name} has no games left this matchup"
        elif lost is None:
            head += f"; {mv.drop.name} has {games} game{'s' if games != 1 else ''} left after that"
        else:
            n = len(lost)
            head += (f"; {mv.drop.name} would have started {n} of his {games} game{'s' if games != 1 else ''} "
                     f"left" + (f" ({', '.join(_wd(d) for d in lost)})" if lost else ""))
    elif mv.to_ir is not None:
        head += f"; {mv.to_ir.name} goes to IR, no drop"
    else:
        head += "; open roster spot, no drop"
    return head


CHECKLIST_LIMIT = 3


def today_checklist(plan: WeekPlan, day: date, limit: int = CHECKLIST_LIMIT) -> list[str]:
    """Up to ``limit`` concrete actions for ``day`` from the plan, most important first: the
    adds / claims scheduled that day, IR and injury actions, then the lineup changes and a goalie
    check ("Start Nylander (for Stenberg, no game)")."""
    if not plan.available:
        return []
    out: list[str] = []
    for m in plan.moves:
        if m.day == day:
            drop = f", drop {m.drop.name}" if m.drop else (f", {m.to_ir.name} to IR" if m.to_ir else "")
            out.append(f"{'Claim' if m.claim else 'Add'} {m.add.name}{drop}: {m.headline or f'+{m.gain_pts:.1f} pts'}")
    for a in plan.today:
        out.append(f"{a.title}: {a.headline}" if a.headline else a.title)
    line = next((x for x in plan.days if x.day == day), None)
    if line is not None:
        if line.ins:
            out.append("After quick lineup, start " + ", ".join(line.ins))
        if line.outs:
            out.append("After quick lineup, bench " + ", ".join(line.outs))
        if line.pregame and line.goalies:
            out.append("Check goalie confirmations: " + ", ".join(line.goalies))
    return out[:limit]


def plan_summary(plan: WeekPlan, day: date) -> list[str]:
    """Morning-digest lead lines: win probability with quick lineup -> with the plan, moves left,
    then "Quick lineup each day; also do:" today's actions."""
    if not plan.available:
        return []
    head = f"This week vs {plan.opponent or 'opponent'}"
    if plan.win_as_set is not None and plan.win_plan is not None:
        head += f": win {plan.win_as_set:.0%} with quick lineup -> {plan.win_plan:.0%} with the plan"
    head += f" ({plan.proj_plan:.1f} pts remaining vs {plan.proj_as_set:.1f} with quick lineup"
    head += f"; moves left {plan.moves_left})" if plan.moves_left is not None else ")"
    from .plan_memory import since_line

    lead = [head] + ([s] if (s := since_line(plan)) else [])
    actions = today_checklist(plan, day)
    if not actions:
        return lead + ["Today: quick lineup is enough"]
    return lead + ["Quick lineup each day; also do:"] + [f"Today: {a}" for a in actions]


def _pair_outs(ins: list[Player], prev: list[Player], playing: set[str]) -> list[tuple[Player, Player | None]]:
    """Pair each player coming into the lineup with one leaving it (same slot type first,
    players without a game before benched ones)."""
    pool = sorted(prev, key=lambda p: (p.cid in playing, p.name))
    out = []
    for p in ins:
        mate = next((q for q in pool if shares_slot(p, q)), None)
        if mate is not None:
            pool.remove(mate)
        out.append((p, mate))
    return out


# --------------------------------------------------------------------------- entry point

def build_week_plan(ctx: LeagueContext, values: Mapping[str, Any], *, dynasty_values: Mapping[str, Any] | None = None,
                    recs: list[Recommendation] | None = None, matchup: Any = None, provider: Any = None,
                    history: Any = None, goalies: Any = None, data_dir: Any = None,
                    now: datetime | None = None) -> WeekPlan:
    """The weekly action plan (module docstring). ``recs`` (the advise output) supplies today's
    lineup / injury items (computed here when None); ``matchup`` (``analysis.matchup.current_matchup``)
    the score and opponent (computed with ``provider`` when None); ``goalies``
    (``analysis.goalie_plan.goalie_week``) the goalie lines (computed when None)."""
    now = now or datetime.now(timezone.utc)
    team = ctx.my_team
    plan = WeekPlan(league=ctx.provider, my_team=team.name)
    if not ctx.schedule or _lock(ctx) != "daily":
        plan.available = False
        plan.notes.append("The weekly plan needs a daily-lineup league with the NHL schedule loaded.")
        return plan
    days = plan_days(ctx)
    if matchup is None:
        try:
            from ..analysis.matchup import current_matchup

            matchup = current_matchup(ctx, provider, values, streaming=False)
        except Exception as e:  # noqa: BLE001 - the plan works without the opponent
            plan.notes.append(f"Matchup unavailable ({type(e).__name__}: {e}); no win probabilities")
    if matchup is not None:
        plan.opponent = matchup.opponent_team
        plan.start, plan.end = matchup.start, matchup.end
        plan.my_so_far, plan.their_so_far = matchup.my_points_so_far, matchup.their_points_so_far
        plan.their_remaining = matchup.their_projected_remaining
        plan.stance = matchup.stance
        if matchup.end is not None:
            days = [d for d in days if d <= matchup.end] or days
    plan.days_left = len(days)
    stance = plan.stance or "even"

    # today's items from the existing recommenders
    if recs is None:
        recs = []
        try:
            from .advise import advise

            recs = advise(ctx, values, dynasty_values=dynasty_values, history=history,
                          include=("lineup", "injuries"))
        except Exception as e:  # noqa: BLE001
            plan.notes.append(f"Lineup / injury advice unavailable: {e}")
    ir_items: list[PlanAction] = []
    for r in recs:
        if r.kind not in ("lineup", "injury"):
            continue
        codes = {x.code for x in r.reasons}
        if r.title.startswith("Hold ") or ("HOLD_VERDICT" in codes and not r.drop and not r.add):
            plan.watch.append(r.headline or r.title)
            continue
        ir = "IR_SWAP" in codes or r.title.startswith("Activate ")
        if r.kind == "lineup" and not (ir or "RETURN_DUE" in codes):
            continue                     # start/sit and benching the injured: quick lineup does it
        item = PlanAction(kind=r.kind, title=r.title, headline=r.headline,
                          gain=None if ir else r.predicted_gain, gain_units=None if ir else r.gain_units)
        plan.today.append(item)
        if ir:
            ir_items.append(item)

    # IR swaps / activations change who is available: apply them before scheduling
    swapped = _apply_ir_actions(team, recs)
    tl = _Timeline(ctx, swapped, values, days, now)
    plan.proj_frozen = sum(tl.as_set(d) for d in days)
    plan.proj_lineup = tl.total([])
    plan.proj_as_set = plan.proj_lineup      # baseline: quick lineup each day
    if ir_items and swapped is not team:     # their matchup points: best daily lineups with vs without them
        before = _Timeline(ctx, team, values, days, now).total([])
        ir_items[0].gain, ir_items[0].gain_units = round(plan.proj_lineup - before, 2), "matchup_pts"

    from .base import dynasty_map

    dyn = dynasty_map(ctx, values, dynasty_values)
    left = moves_left(ctx)
    plan.moves_left = left
    max_moves = UNLIMITED_MOVES_CAP if left is None else left
    if stance == "protect" and max_moves >= 2:
        max_moves -= 1
        plan.notes.append("Protect stance: one move is kept in reserve for an injury.")
    # plan memory: earlier plans for this matchup period (data_dir given: the CLI, dashboard, harness)
    from . import plan_memory

    snaps = plan_memory.load_snapshots(data_dir, plan, ctx) if data_dir is not None else None
    kept: dict[tuple, str] = {}
    txs: list[_Tx] = []
    if max_moves > 0:
        pairs, notes = _candidates(ctx, values, dynasty_values)
        plan.notes.extend(notes)
        min_pts = MIN_MOVE_PTS.get(stance, 1.0)
        txs = _search(ctx, tl, pairs, max_moves, min_pts, now, values, dyn)
        if snaps:      # keep the last plan's steps unless the new one is clearly better
            txs, kept = plan_memory.stabilize(ctx, tl, txs, snaps[-1], pairs, max_moves, min_pts, now)
    elif left == 0:
        plan.notes.append("No moves left this period: the plan only sets lineups.")
    plan.moves_planned = len(txs)
    plan.proj_plan = tl.total(txs)
    if left != 0:      # season-upgrade claims on waivers the plan does not schedule (recommend/claims.py)
        try:
            from .claims import claim_watch_lines

            plan.claim_watch.extend(claim_watch_lines(ctx, values, dynasty_values, {t.add.cid for t in txs}))
        except Exception as e:  # noqa: BLE001 - the watch list is optional
            plan.notes.append(f"Claim check unavailable: {e}")

    # win probabilities: quick lineup (best daily lineups, the baseline) -> each move in order
    plan.win_frozen = _win(tl, [], matchup, as_set=True)
    plan.win_lineup = _win(tl, [], matchup)
    plan.win_as_set = plan.win_lineup
    per_day = tl.per_day(txs)
    prev_total, prev_win = plan.proj_lineup, plan.win_lineup
    for i, t in enumerate(txs):
        upto = _upto(txs, i)
        tot = tl.total(upto)
        win = _win(tl, upto, matchup)
        starts = [d for d, (_, chosen) in zip(days, per_day) if t.add.cid in chosen]
        # nights the dropped player would have started without this move (the earlier moves as planned)
        lost = None
        if t.drop is not None:
            without = tl.per_day(_upto(txs[:i] + txs[i + 1:], i - 1))
            lost = [d for d, (_, chosen) in zip(days, without) if d >= t.effective and t.drop.cid in chosen]
        mv = PlanMove(day=t.day, effective=t.effective, add=t.add, drop=t.drop, to_ir=t.to_ir,
                      gain_pts=round(tot - prev_total, 2),
                      win_delta=(win - prev_win) if win is not None and prev_win is not None else None,
                      starts=starts, claim=t.add.waiver_until is not None and t.add.waiver_until >= ctx.as_of,
                      market=(market_suffix(t.rec).strip(" ·") or None) if t.rec is not None else None)
        mv.headline = _move_headline(tl, mv, lost)
        if t.rec is not None and t.rec.reasons:
            mv.reasons = [x.text for x in t.rec.reasons if x.code in ("WAIVER_CLAIM", "CHURN_GUARD")]
        if t.drop is not None:      # long-term change against the player this step actually drops
            mv.reasons.insert(0, _long_term(ctx, values, dyn, t.add, t.drop)[1])
        mv.timing, mv.day = _timing(ctx, mv)
        if plan_memory.move_key(mv) in kept:
            mv.status = "kept"
        plan.moves.append(mv)
        prev_total, prev_win = tot, win
    plan.win_plan = prev_win
    _chains(plan, ctx, values, dyn, tl)
    if snaps is not None:
        plan.since = plan_memory.compare(plan, ctx, snaps, kept, tl, txs, values, dyn, now)
        if plan.since is not None:
            plan.watch.extend(plan.since.long_term)
        plan_memory.save_snapshot(data_dir, plan, ctx, now)

    # goalie lines
    if goalies is None:
        try:
            from ..analysis.goalie_plan import goalie_week

            goalies = goalie_week(ctx, values, streams=False, data_dir=data_dir)
        except Exception as e:  # noqa: BLE001
            plan.notes.append(f"Goalie planner unavailable: {e}")
    _day_lines(plan, ctx, tl, txs, per_day, goalies)
    plan.notes.extend(_assumption_notes(ctx))
    return plan


def _slot_label(p: Player) -> str:
    pos = [x for x in p.positions if x not in ("F", "UTIL", "BN")]
    return "G" if p.is_goalie else ("/".join(pos[:2]) if pos else "F")


def _chains(plan: WeekPlan, ctx: LeagueContext, values: Mapping[str, Any], dyn: Mapping[str, float] | None,
            tl: _Timeline) -> None:
    """Tell re-streams as one story: the first pickup is a rental, the later step that drops him a
    swap (why: his games are done, the move would otherwise expire, the keeper value), plus the
    "Net this week" lines (who is gone, who stays, long-term change, points)."""
    by_add = {m.add.cid: m for m in plan.moves}
    n = 0
    for i, m in enumerate(plan.moves):
        prev = by_add.get(m.drop.cid) if m.drop is not None else None
        if prev is None or prev is m:
            continue
        if prev.chain is None:
            n += 1
            prev.chain, prev.role = n, "rental"
            span = f"{_wd(prev.starts[0])}-{_wd(prev.starts[-1])}" if prev.starts else _wd(prev.effective)
            k = len(prev.starts)
            prev.note = (f"Rent {prev.add.name} {span}: {k} night{'s' if k != 1 else ''} your "
                         f"{_slot_label(prev.add)} slot would otherwise miss")
        m.chain, m.role = prev.chain, "swap"
        later = [d for d in tl.days if d >= m.effective and tl.value(prev.add, d) > 0]
        why = [f"{prev.add.name} has no games left" if not later
               else f"{prev.add.name} has {len(later)} game{'s' if len(later) != 1 else ''} left"]
        last = i == len(plan.moves) - 1 and plan.moves_left is not None and plan.moves_left == len(plan.moves)
        if last:
            reset = (ctx.period_end + timedelta(days=1)) if getattr(ctx, "period_end", None) else None
            why.append("this is your last move this week and unused moves don't carry over"
                       + (f" (reset {_wd(reset)} {reset:%b} {reset.day})" if reset else ""))
        if dyn is not None and dyn.get(m.add.cid, 0.0) > dyn.get(prev.add.cid, 0.0):
            why.append(f"{m.add.name} is the better keeper")
        m.note = f"Then swap {prev.add.name} for {m.add.name} {_wd(m.effective)}: " + "; ".join(why)
    if not plan.moves:
        return
    added = {m.add.cid: m.add for m in plan.moves}
    dropped = {m.drop.cid: m.drop for m in plan.moves if m.drop is not None}
    stays = [p for c, p in added.items() if c not in dropped]
    gone = [p for c, p in dropped.items() if c not in added]
    rented = [p for c, p in added.items() if c in dropped]
    if dyn is not None:
        delta = sum(dyn.get(p.cid, 0.0) for p in stays) - sum(dyn.get(p.cid, 0.0) for p in gone)
        lt = f"long term {delta:+.1f} dynasty value"
    else:
        delta = sum(float(getattr(values.get(p.cid), "fpg_season", 0.0) or 0.0) for p in stays) \
            - sum(float(getattr(values.get(p.cid), "fpg_season", 0.0) or 0.0) for p in gone)
        lt = f"rest of season {delta:+.2f} FPG"
    line = (f"Net this week: {', '.join(p.name for p in gone) or 'nobody'} gone; "
            f"{', '.join(p.name for p in stays) or 'nobody'} stay{'s' if len(stays) == 1 else ''}")
    if rented:
        line += f"; {', '.join(p.name for p in rented)} rented and released"
    plan.net_week.append(f"{line}; {lt}; {plan.moves_gain:+.1f} pts this matchup")


def _apply_ir_actions(team: FantasyTeam, recs: list[Recommendation]) -> FantasyTeam:
    """``team`` after today's IR swaps (activate X, Y to IR) and plain activations with room."""
    from ..models import RosterSlot

    act: set[str] = set()
    to_ir: set[str] = set()
    for r in recs:
        if r.kind != "lineup" or not r.add:
            continue
        codes = {x.code for x in r.reasons}
        if "IR_SWAP" in codes and r.subjects:
            act.add(r.add[0].cid)
            to_ir.add(r.subjects[0].cid)
        elif r.title.startswith("Activate ") and "ROSTER_FULL" not in codes:
            act.add(r.add[0].cid)
    if not act and not to_ir:
        return team
    slots = []
    for s in team.slots:
        if s.player is not None and s.player.cid in act and s.slot == "IR":
            slots.append(RosterSlot(slot="BN", player=s.player, starting=False))
        elif s.player is not None and s.player.cid in to_ir:
            slots.append(RosterSlot(slot="IR", player=s.player, starting=False))
        else:
            slots.append(s)
    return team.model_copy(update={"slots": slots})


def _quick_choice(tl: _Timeline, d: date, players: list[Player]) -> tuple[float, frozenset[str]]:
    """What quick lineup / auto-set would start on ``d``: players with a game, ranked by their season
    value (what a platform's own projections rank by: no roles, matchups or goalie start odds),
    scored at our day values. Locked players today stay where they are, as in ``_Timeline.day``."""
    today = d == tl.ctx.as_of
    pool = [p for p in players if (tl.value(p, d) > 0 or (today and p.cid in tl.locked))
            and not (today and p.cid in tl.locked and p.cid not in tl.starters)]

    def rank(p: Player) -> float:
        pv = tl.values.get(p.cid)
        base = float(getattr(pv, "fpg_season", 0.0) or 0.0)
        return base + (LOCK_BONUS if today and p.cid in tl.locked else 0.0)
    _, assign = _dp(pool, tl.slots, lambda p, t: rank(p))
    chosen = frozenset(assign.values())
    return sum(tl.value(p, d) for p in pool if p.cid in chosen), chosen


def _day_lines(plan: WeekPlan, ctx: LeagueContext, tl: _Timeline, txs: list[_Tx],
               per_day: list[tuple[float, frozenset[str]]], goalies: Any) -> None:
    rows = {r.cid: r for r in [*getattr(goalies, "mine", []), *getattr(goalies, "free_agents", [])]}
    base_days = tl.per_day([])
    for i, d in enumerate(tl.days):
        line = PlanDayLine(day=d, pts_plan=round(per_day[i][0], 2), pts_as_set=round(base_days[i][0], 2),
                           pts_frozen=round(tl.as_set(d), 2))
        line.moves = [m for m in plan.moves if m.day == d]
        roster = {p.cid: p for p in tl.roster(d, txs)}
        chosen = per_day[i][1]
        playing = {c for c in roster if tl.value(roster[c], d) > 0}
        # starts quick lineup would get wrong: only when the best lineup beats it by QUICK_EDGE_MIN
        q_pts, quick = _quick_choice(tl, d, list(roster.values()))
        if per_day[i][0] - q_pts >= QUICK_EDGE_MIN:
            ins = [roster[c] for c in chosen if c not in quick]
            outs = [roster[c] for c in quick if c not in chosen]
            for p, mate in _pair_outs(ins, outs, playing):
                if mate is None:
                    line.ins.append(f"{p.name} (open slot)")
                    continue
                line.ins.append(f"{p.name} over {mate.name} ({tl.value(p, d):.1f} vs {tl.value(mate, d):.1f} pts)")
                outs.remove(mate)
            line.outs = [p.name for p in outs]
            line.quick_ok = False
        # goalies in the plan's lineup that day (incl. goalies the plan adds; dropped ones are gone),
        # with the goalie planner's start probability where it has the row
        for c in chosen:
            if not roster[c].is_goalie:
                continue
            row = rows.get(c)
            game = next((x for x in (row.games if row else []) if x.day == d), None)
            if game is None:
                line.goalies.append(roster[c].name)
                line.pregame = True
                continue
            line.goalies.append(f"{row.name} {game.start_prob:.0%} {game.opponent}")
            if game.start_prob < PREGAME_CONFIRMED:
                line.pregame = True
        line.note = _below_note(plan, ctx, d, line, base_days[i][1], roster)
        line.text = _day_text(line)
        plan.days.append(line)


def _below_note(plan: WeekPlan, ctx: LeagueContext, d: date, line: PlanDayLine, quick: frozenset[str],
                roster: Mapping[str, Player]) -> str:
    """Why the plan projects below quick lineup on ``d``: players the plan dropped who quick lineup
    (no moves) would have started that day."""
    if line.pts_plan >= line.pts_as_set - 0.05:
        return ""
    gone = [m for m in plan.moves if m.drop is not None and m.effective <= d
            and m.drop.cid in quick and m.drop.cid not in roster]
    if not gone:
        return ""
    when = "today" if d == ctx.as_of else _wd(d)
    who = ", ".join(f"{m.drop.name} (dropped {_wd(m.day)})" for m in gone)
    text = f"Lower than quick lineup {when}: {who} would have played"
    if plan.moves_gain > 0:
        adds = list(dict.fromkeys(m.add.name for m in gone))
        whose = f"{adds[0]}'s extra nights make" if len(adds) == 1 else "the pickups' extra nights make"
        text += f"; {whose} up for it ({plan.moves_gain:+.1f} for the week)"
    return text


def _day_text(line: PlanDayLine) -> str:
    parts = []
    for m in line.moves:
        drop = f", drop {m.drop.name}" if m.drop else (f", {m.to_ir.name} to IR" if m.to_ir else "")
        when = "" if m.effective == m.day else f" (counts from {_wd(m.effective)})"
        parts.append(f"{'Claim' if m.claim else 'Add'} {m.add.name}{drop}{when}, then quick lineup")
    if line.ins:
        parts.append("after quick lineup start " + ", ".join(line.ins))
    if line.outs:
        parts.append("bench " + ", ".join(line.outs))
    if line.goalies:
        parts.append("G: " + ", ".join(line.goalies))
    if not line.moves and line.quick_ok and not line.pregame:
        parts.insert(0, "Quick lineup is fine")
    text = "; ".join(parts) if parts else "Quick lineup is fine"
    if line.pregame:
        text += " · check goalies at 5 pm"
    if line.note:
        text += f" · {line.note}"
    return text


def _assumption_notes(ctx: LeagueContext) -> list[str]:
    out = []
    if ctx.provider == "fantrax":
        out.append("Fantrax free-agent adds count the same day when made before the day's first game "
                   "(else from the next day); waiver claims from the day after they clear.")
    return out


# --------------------------------------------------------------------------- archive

def archive_plan(plan: WeekPlan, data_dir: Path | str, league_id: str | None = None) -> str | None:
    """Write ``archive/plan-<league>-<date>.json`` (one per day; rewritten when it changes) so the
    harness can grade the prediction against the final score. Returns the write status or None."""
    if not plan.available:
        return None
    from ..backtest.archive import _write_dedup, archive_dir

    day = plan.days[0].day if plan.days else date.today()
    path = archive_dir(data_dir) / f"plan-{plan.league}-{day.isoformat()}.json"
    body = plan.to_json()
    payload = {"version": 1, "provider": plan.league, "league_id": league_id, "as_of": day.isoformat(),
               "created": datetime.now(timezone.utc).isoformat(timespec="seconds"),
               "plan": json.loads(json.dumps(body, default=str))}
    try:
        return _write_dedup(path, payload, "plan")
    except OSError:
        return None


__all__ = ["MIN_MOVE_PTS", "PlanAction", "PlanDayLine", "PlanMove", "WeekPlan", "archive_plan", "build_week_plan"]
