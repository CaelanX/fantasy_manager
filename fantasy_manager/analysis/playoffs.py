"""Roster-aware fantasy-playoff planner (``fm playoffs``, the dashboard's Playoffs section).

Built on ``schedule_grid.playoff_weeks`` (the league's playoff periods and every NHL team's games
in them):

* **Per-game value in the playoffs.** ``playoff_per_game``: a player's healthy per-game value
  (``PlayerValue.fpg``) times his start share (goalies). The playoffs are months away, so this
  week's availability, role bumps and Daily Faceoff starts are left out. A player who is out
  counts from his estimated ``return_date``. With no date he is assumed back, except on LTIR.
* **Projected playoff points.** Each team's best lineup each playoff day: only players with a
  game compete for the starting slots (``recommend.lineup`` solver). Weekly-lock leagues set one
  lineup per period, valued at the period's games. Every league team is projected the same way,
  which gives my rank and the league-average starts by position (F / D / G).
* **Hold.** My players by their contribution: the lineup's playoff points with him minus without
  him.
* **Sell.** My players whose NHL team plays at least ``SELL_GAMES_BELOW`` fewer playoff games
  than the average NHL team, by the points that schedule costs (per-game x missing games).
* **Acquire.** Free agents and other teams' players by the playoff points they add to my
  lineup (no drop). Only the ``CANDIDATES`` best by playoff games x per-game x off-night bonus are
  re-solved.
* **Trade deadline.** ESPN ``settings.trade_deadline`` (ms), Fantrax Rules page "Trade Deadline
  Date"; days left from today.
"""
from __future__ import annotations

import re
from datetime import date, datetime, timedelta
from typing import Any, Mapping, Sequence

from pydantic import BaseModel, Field

from ..matching.normalize import normalize_team
from ..models import LeagueContext, Player
from .schedule_grid import (OFFNIGHT_THRESHOLD, UNAVAILABLE, PlayoffWeeks, _group, offnight_bonus, per_day,
                            playoff_weeks, window_counts)

TARGETS = 10
CANDIDATES = 60          # acquisition candidates re-solved in my lineup (best naive playoff points)
SELL_GAMES_BELOW = 2     # playoff games below the average NHL team that make a player a sell candidate
GROUPS = ("F", "D", "G")
DEADLINE_LABEL = "Trade Deadline Date"


class PlayoffPlayer(BaseModel):
    cid: str
    name: str
    team: str | None
    pos: str
    group: str
    owner: str | None = None          # fantasy team (None: free agent)
    status: str = "healthy"
    games: list[int] = Field(default_factory=list)   # per playoff period
    total: int = 0
    offnights: int = 0
    b2b: int = 0
    vs_avg: float = 0.0               # playoff games minus the average NHL team's
    per_game: float = 0.0
    naive: float = 0.0                # per-game x games x (1 + bonus x off-nights)
    points: float = 0.0               # my lineup's playoff points with him minus without him
    starts: int | None = None         # playoff starts in my lineup (my players)
    cost: float | None = None         # sell: points the weak schedule costs vs an average one


class GroupStarts(BaseModel):
    group: str
    mine: float
    league_avg: float


class PlayoffPlan(BaseModel):
    periods: list[Any] = Field(default_factory=list)
    source: str = ""
    notes: list[str] = Field(default_factory=list)
    days: int = 0
    avg_team_games: float = 0.0
    lock: str = "daily"
    my_team: str = ""
    my_points: float = 0.0
    league_avg_points: float = 0.0
    rank: int | None = None
    n_teams: int = 0
    groups: list[GroupStarts] = Field(default_factory=list)
    hold: list[PlayoffPlayer] = Field(default_factory=list)
    sell: list[PlayoffPlayer] = Field(default_factory=list)
    acquire_fa: list[PlayoffPlayer] = Field(default_factory=list)
    acquire_trade: list[PlayoffPlayer] = Field(default_factory=list)
    deadline: date | None = None
    deadline_source: str | None = None
    days_to_deadline: int | None = None


# --------------------------------------------------------------------------- values

def playoff_per_game(pv: Any) -> float:
    """Expected points per NHL team game in the fantasy playoffs: healthy FPG x start share."""
    if pv is None:
        return 0.0
    fpg = getattr(pv, "fpg", None) or 0.0
    share = getattr(pv, "start_share", None)
    return max(0.0, float(fpg) * (float(share) if share is not None else 1.0))


def available_in_playoffs(p: Player, pv: Any, day: date) -> bool:
    """An injured player counts from his estimated return date; without one he is assumed back
    by then, except on LTIR."""
    if p.status not in UNAVAILABLE:
        return True
    ret = getattr(pv, "return_date", None)
    if ret is not None:
        return day >= ret
    return p.status != "ltir"


def _days(periods: Sequence[Any]) -> list[date]:
    out: list[date] = []
    for per in periods:
        out += [per.start + timedelta(days=i) for i in range((per.end - per.start).days + 1)]
    return out


class _Solver:
    """Best playoff lineup per day (or per period in weekly-lock leagues) for a pool of players."""

    def __init__(self, ctx: LeagueContext, values: Mapping[str, Any], periods: Sequence[Any], lock: str):
        from ..recommend.lineup import starting_slots

        self.ctx, self.values = ctx, values
        self.slots = starting_slots(ctx.roster_shape)
        self.lock = lock
        days = _days(periods)
        # a "unit" is a day (daily lineups) or a period (weekly lock); each unit -> its days
        self.units: list[list[date]] = [[d] for d in days] if lock == "daily" else [_days([p]) for p in periods]
        self._pg: dict[str, float] = {}

    def per_game(self, p: Player) -> float:
        if p.cid not in self._pg:
            self._pg[p.cid] = playoff_per_game(self.values.get(p.cid))
        return self._pg[p.cid]

    def games_in(self, p: Player, unit: Sequence[date]) -> int:
        """His games in ``unit`` that he is available for."""
        games = set(self.ctx.schedule.get(normalize_team(p.team) or "", ()) or ())
        pv = self.values.get(p.cid)
        return sum(1 for d in unit if d in games and available_in_playoffs(p, pv, d))

    def unit_value(self, p: Player, unit: Sequence[date]) -> float:
        return self.games_in(p, unit) * self.per_game(p)

    def solve_unit(self, pool: Sequence[Player], unit: Sequence[date]) -> tuple[float, set[str]]:
        from ..recommend.lineup import _dp

        vals = {p.cid: self.unit_value(p, unit) for p in pool}
        live = [p for p in pool if vals[p.cid] > 0]
        if not live:
            return 0.0, set()
        total, assign = _dp(live, self.slots, lambda p, t: vals[p.cid])
        return total, set(assign.values())

    def solve(self, pool: Sequence[Player]) -> tuple[float, list[float], dict[str, int]]:
        """(total points, per-unit points, playoff starts per cid)."""
        by_cid = {p.cid: p for p in pool}
        per_unit: list[float] = []
        starts: dict[str, int] = {}
        for unit in self.units:
            total, chosen = self.solve_unit(pool, unit)
            per_unit.append(total)
            for c in chosen:
                starts[c] = starts.get(c, 0) + self.games_in(by_cid[c], unit)
        return sum(per_unit), per_unit, starts

    def plays_in(self, p: Player, unit: Sequence[date]) -> bool:
        games = set(self.ctx.schedule.get(normalize_team(p.team) or "", ()) or ())
        return any(d in games for d in unit)

    def delta(self, pool: Sequence[Player], per_unit: Sequence[float], add: Player | None = None,
              remove: Player | None = None) -> float:
        """Total change from adding ``add`` / removing ``remove``: only the units he plays in."""
        who = add or remove
        assert who is not None
        new_pool = [p for p in pool if remove is None or p.cid != remove.cid] + ([add] if add else [])
        out = 0.0
        for i, unit in enumerate(self.units):
            if self.plays_in(who, unit):
                out += self.solve_unit(new_pool, unit)[0] - per_unit[i]
        return out


# --------------------------------------------------------------------------- trade deadline

_DATE_FORMATS = ("%b %d, %Y", "%B %d, %Y", "%b %d %Y", "%B %d %Y", "%Y-%m-%d", "%m/%d/%Y", "%m/%d/%y",
                 "%b %d/%y", "%d %b %Y")


def parse_deadline(text: str | None) -> date | None:
    """Best-effort date out of a Rules-page value such as "Thu Mar 4, 2027 12:00PM EST"."""
    if not text:
        return None
    s = re.sub(r"^\s*(mon|tue|wed|thu|fri|sat|sun)[a-z]*\.?,?\s+", "", str(text).strip(), flags=re.I)
    s = re.sub(r"\s+\d{1,2}:\d{2}.*$", "", s).strip().rstrip(",")
    for fmt in _DATE_FORMATS:
        try:
            return datetime.strptime(s, fmt).date()
        except ValueError:
            continue
    m = re.search(r"\d{4}-\d{2}-\d{2}", str(text))
    if m:
        return date.fromisoformat(m.group(0))
    return None


def trade_deadline(ctx: LeagueContext, provider: Any) -> tuple[date | None, str | None]:
    if provider is None:
        return None, None
    try:
        if ctx.provider == "espn":
            from .schedule_grid import espn_league

            ms = getattr(espn_league(provider).settings, "trade_deadline", None)
            if isinstance(ms, (int, float)) and ms > 0:
                return datetime.fromtimestamp(ms / 1000).date(), "ESPN league settings"
        elif ctx.provider == "fantrax":
            rules = getattr(provider, "rules", None)
            raw = rules.get(DEADLINE_LABEL) if rules is not None else None
            d = parse_deadline(raw)
            if d is not None:
                return d, "Fantrax Rules page"
            if raw:                     # e.g. "Not Used": no deadline in this league
                return None, f"Fantrax Rules page: {raw}"
    except Exception:  # noqa: BLE001 - the countdown is optional
        return None, None
    return None, None


# --------------------------------------------------------------------------- the plan

def _row(ctx: LeagueContext, p: Player, po: PlayoffWeeks, gpd: Mapping[date, int], solver: _Solver,
         owner: str | None, threshold: int) -> PlayoffPlayer | None:
    team = normalize_team(p.team)
    dates = ctx.schedule.get(team or "")
    if not dates:
        return None
    cells = [window_counts(dates, gpd, per.start, per.end, threshold) for per in po.periods]
    games = [c[0] for c in cells]
    total, off, b2b = sum(games), sum(c[1] for c in cells), sum(c[2] for c in cells)
    pg = solver.per_game(p)
    return PlayoffPlayer(cid=p.cid, name=p.name, team=team, pos="/".join(x for x in p.positions if x != "F") or _group(p),
                         group=_group(p), owner=owner, status=p.status, games=games, total=total, offnights=off,
                         b2b=b2b, vs_avg=round(total - po.avg_total, 2), per_game=round(pg, 3),
                         naive=round(pg * total * (1 + offnight_bonus() * off), 2))


def playoff_plan(ctx: LeagueContext, values: Mapping[str, Any], provider: Any = None,
                 weeks: PlayoffWeeks | None = None, limit: int = TARGETS,
                 threshold: int = OFFNIGHT_THRESHOLD) -> PlayoffPlan:
    """The roster-aware fantasy-playoff plan (see the module docstring)."""
    from ..recommend.lineup import _lock

    po = weeks or playoff_weeks(ctx, provider, threshold=threshold)
    plan = PlayoffPlan(periods=po.periods, source=po.source, notes=list(po.notes), avg_team_games=po.avg_total)
    deadline, src = trade_deadline(ctx, provider)
    plan.deadline, plan.deadline_source = deadline, src
    if deadline is not None:
        plan.days_to_deadline = (deadline - ctx.as_of).days
    if not po.periods or not ctx.schedule:
        return plan
    try:
        mine = ctx.my_team
    except LookupError:
        plan.notes.append("No team flagged as mine")
        return plan
    plan.lock = _lock(ctx)
    plan.days = len(_days(po.periods))
    plan.my_team = mine.name
    solver = _Solver(ctx, values, po.periods, plan.lock)
    gpd = per_day(ctx)

    # every team's projected playoff points and starts by position
    totals: dict[str, float] = {}
    starts_by: dict[str, dict[str, float]] = {}
    my_units: list[float] = []
    my_starts: dict[str, int] = {}
    for t in ctx.teams:
        total, units, starts = solver.solve(t.players)
        totals[t.team_id] = total
        by_cid = {p.cid: p for p in t.players}
        starts_by[t.team_id] = {g: float(sum(n for c, n in starts.items() if _group(by_cid[c]) == g)) for g in GROUPS}
        if t.team_id == mine.team_id:
            my_units, my_starts = units, starts
    plan.my_points = round(totals.get(mine.team_id, 0.0), 1)
    plan.n_teams = len(totals)
    plan.league_avg_points = round(sum(totals.values()) / max(1, len(totals)), 1)
    plan.rank = 1 + sum(1 for k, v in totals.items() if k != mine.team_id and v > totals.get(mine.team_id, 0.0))
    plan.groups = [GroupStarts(group=g, mine=starts_by.get(mine.team_id, {}).get(g, 0.0),
                               league_avg=round(sum(s.get(g, 0.0) for s in starts_by.values()) / max(1, len(starts_by)), 1))
                   for g in GROUPS if any(s.get(g) for s in starts_by.values())]

    # hold / sell: my players
    pool = list(mine.players)
    hold: list[PlayoffPlayer] = []
    sell: list[PlayoffPlayer] = []
    for p in pool:
        row = _row(ctx, p, po, gpd, solver, mine.name, threshold)
        if row is None:
            continue
        row.starts = my_starts.get(p.cid, 0)
        row.points = round(-solver.delta(pool, my_units, remove=p), 2)
        hold.append(row)
        if row.vs_avg <= -SELL_GAMES_BELOW and row.per_game > 0:
            row_s = row.model_copy(update={"cost": round(row.per_game * -row.vs_avg, 2)})
            sell.append(row_s)
    plan.hold = sorted(hold, key=lambda r: (-r.points, -r.naive, r.name))
    plan.sell = sorted(sell, key=lambda r: (-(r.cost or 0.0), r.name))[:limit]

    # acquire: free agents and other teams' players, by the playoff points they add to my lineup
    owners = {p.cid: t.name for t in ctx.teams if t.team_id != mine.team_id for p in t.players}
    mine_ids = {p.cid for p in pool}
    cands: list[tuple[Player, PlayoffPlayer]] = []
    for p in list(ctx.free_agents) + [q for t in ctx.teams if t.team_id != mine.team_id for q in t.players]:
        if p.cid in mine_ids or (p.status == "ltir" and getattr(values.get(p.cid), "return_date", None) is None):
            continue
        row = _row(ctx, p, po, gpd, solver, owners.get(p.cid), threshold)
        if row is not None and row.naive > 0:
            cands.append((p, row))
    fa = sorted((c for c in cands if c[1].owner is None), key=lambda c: -c[1].naive)[:CANDIDATES // 2]
    tr = sorted((c for c in cands if c[1].owner is not None), key=lambda c: -c[1].naive)[:CANDIDATES // 2]
    for p, row in fa + tr:
        row.points = round(solver.delta(pool, my_units, add=p), 2)
    key = lambda c: (-c[1].points, -c[1].naive, c[1].name)  # noqa: E731
    plan.acquire_fa = [r for _, r in sorted(fa, key=key) if r.points > 0][:limit]
    plan.acquire_trade = [r for _, r in sorted(tr, key=key) if r.points > 0][:limit]
    return plan


def plan_json(plan: PlayoffPlan) -> dict[str, Any]:
    return plan.model_dump(mode="json")


__all__ = ["PlayoffPlan", "PlayoffPlayer", "GroupStarts", "playoff_plan", "playoff_per_game",
           "available_in_playoffs", "parse_deadline", "trade_deadline", "plan_json"]
