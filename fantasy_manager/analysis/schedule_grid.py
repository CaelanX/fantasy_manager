"""Season schedule grid, weekly streaming planner and fantasy-playoff schedule strength.

Everything is computed from the NHL schedule already loaded on the context (``ctx.schedule``:
team -> regular-season game dates, ``ctx.games_per_day``, ``ctx.opponents``), so no function here
touches the network except the league-calendar readers, which ask the provider for its scoring
periods (best effort: failures become notes and a Monday-Sunday calendar is used instead).

* ``season_grid(ctx, start, end)`` - games, off-night games (team games on nights with fewer than
  8 NHL games) and back-to-backs per NHL team per fantasy week (Monday-Sunday).
* ``team_week_counts(ctx)`` - the same grid for the whole regular season.
* ``week_rows(ctx, week_start)`` - one week as a team x day matrix (opponent labels, off-nights).
* ``streaming_targets(ctx, values, week_start)`` - free agents ranked by projected points over the
  rest of that week (per-game value x games x (1 + off-night bonus x off-night games)) plus the
  NHL teams with the most games left to stream from.
* ``league_calendar(ctx, provider)`` - the league's matchup / scoring periods: ESPN matchup periods
  (``settings.matchup_periods``, daily scoring periods from opening night), Fantrax scoring
  periods (``getTeamRosterInfo`` GAMES_PER_POS ``scoringPeriodList``), else calendar weeks.
* ``playoff_weeks(ctx, provider)`` - the fantasy-playoff periods and every NHL team's games in them
  (the classic "who plays most in the fantasy playoffs" table).
"""
from __future__ import annotations

import os
import re
from datetime import date, datetime, timedelta
from typing import Any, Iterable, Mapping, Sequence

from pydantic import BaseModel, Field

from ..models import LeagueContext, Player

OFFNIGHT_THRESHOLD = 8
UNAVAILABLE = ("out", "ir", "ltir", "suspended")
# Playoff start period when the provider does not say (Fantrax: this league's Rules page says 25).
DEFAULT_PLAYOFF_START: dict[str, int] = {"fantrax": 25}
PLAYOFF_ENV = "FM_PLAYOFF_START_PERIOD"
FALLBACK_PLAYOFF_WEEKS = 3
LIGHT_WEEK_SHARE = 0.4
FANTRAX_PLAYOFF_LABEL = "Playoffs will begin in this Scoring Period"
_FX_PERIOD_RE = re.compile(r"\(?\s*([A-Za-z]{3} \d{1,2}/\d{2})\s*-\s*([A-Za-z]{3} \d{1,2}/\d{2})\s*\)?")


# --------------------------------------------------------------------------- basics

def monday(d: date) -> date:
    """The Monday of ``d``'s week."""
    return d - timedelta(days=d.weekday())


def is_offnight(n_games: int, threshold: int = OFFNIGHT_THRESHOLD) -> bool:
    return 0 < n_games < threshold


def per_day(ctx: LeagueContext) -> dict[date, int]:
    """NHL games per date (``ctx.games_per_day``, else derived from the team schedules)."""
    if ctx.games_per_day:
        return dict(ctx.games_per_day)
    teams: dict[date, int] = {}
    for dates in ctx.schedule.values():
        for d in set(dates):
            teams[d] = teams.get(d, 0) + 1
    return {d: (n + 1) // 2 for d, n in sorted(teams.items())}


def season_bounds(ctx: LeagueContext) -> tuple[date, date] | None:
    """(first, last) regular-season game date in the loaded schedule."""
    ds = [d for dates in ctx.schedule.values() for d in dates]
    if not ds:
        return None
    return min(ds), max(ds)


def default_week(ctx: LeagueContext) -> date:
    """Monday of the current week; the opening week's Monday before the season starts."""
    day = ctx.as_of
    if ctx.season_start and ctx.season_start > day:
        day = ctx.season_start
    return monday(day)


def window_counts(dates: Iterable[date], games_per_day: Mapping[date, int], start: date, end: date,
                  threshold: int = OFFNIGHT_THRESHOLD) -> tuple[int, int, int]:
    """(games, off-night games, back-to-backs) for one team's dates in [start, end]. A back-to-back
    is counted in the window of its second game."""
    all_ds = sorted(set(dates))
    ds = [d for d in all_ds if start <= d <= end]
    prev = set(all_ds)
    games = len(ds)
    off = sum(1 for d in ds if is_offnight(games_per_day.get(d, 0), threshold))
    b2b = sum(1 for d in ds if (d - timedelta(days=1)) in prev)
    return games, off, b2b


# --------------------------------------------------------------------------- season grid

class TeamWeeks(BaseModel):
    team: str
    games: list[int]
    offnights: list[int]
    b2b: list[int]
    total_games: int = 0
    total_offnights: int = 0
    total_b2b: int = 0


class Grid(BaseModel):
    weeks: list[date] = Field(default_factory=list)       # Monday of each week
    week_ends: list[date] = Field(default_factory=list)   # Sunday (clipped to ``end``)
    league_games: list[int] = Field(default_factory=list)  # NHL games per week
    offnight_days: list[int] = Field(default_factory=list)  # nights with < threshold games per week
    teams: list[TeamWeeks] = Field(default_factory=list)   # alphabetical
    threshold: int = OFFNIGHT_THRESHOLD

    def team(self, abbrev: str) -> TeamWeeks | None:
        return next((t for t in self.teams if t.team == abbrev), None)

    def week_index(self, day: date) -> int | None:
        m = monday(day)
        return self.weeks.index(m) if m in self.weeks else None


def season_grid(ctx: LeagueContext, start: date | None = None, end: date | None = None,
                threshold: int = OFFNIGHT_THRESHOLD) -> Grid:
    """Games / off-night games / back-to-backs per NHL team per Monday-Sunday week in
    [start, end] (default: the whole loaded regular season). Empty without a schedule."""
    bounds = season_bounds(ctx)
    if bounds is None:
        return Grid(threshold=threshold)
    start = start or bounds[0]
    end = end or bounds[1]
    if end < start:
        return Grid(threshold=threshold)
    gpd = per_day(ctx)
    weeks: list[tuple[date, date]] = []
    w = monday(start)
    while w <= end:
        weeks.append((max(w, start), min(w + timedelta(days=6), end)))
        w += timedelta(days=7)
    teams = []
    for team in sorted(ctx.schedule):
        dates = ctx.schedule[team]
        cells = [window_counts(dates, gpd, a, b, threshold) for a, b in weeks]
        g, o, bb = ([c[i] for c in cells] for i in range(3))
        teams.append(TeamWeeks(team=team, games=g, offnights=o, b2b=bb, total_games=sum(g),
                               total_offnights=sum(o), total_b2b=sum(bb)))
    league_games, off_days = [], []
    for a, b in weeks:
        days = [a + timedelta(days=i) for i in range((b - a).days + 1)]
        league_games.append(sum(gpd.get(d, 0) for d in days))
        off_days.append(sum(1 for d in days if is_offnight(gpd.get(d, 0), threshold)))
    return Grid(weeks=[monday(a) for a, _ in weeks], week_ends=[b for _, b in weeks], league_games=league_games,
                offnight_days=off_days, teams=teams, threshold=threshold)


def team_week_counts(ctx: LeagueContext, threshold: int = OFFNIGHT_THRESHOLD) -> Grid:
    """The season-long team x week matrix."""
    return season_grid(ctx, threshold=threshold)


# --------------------------------------------------------------------------- one week

class DayCell(BaseModel):
    date: date
    opp: str | None = None      # "BOS" at home, "@BOS" away
    off: bool = False           # off-night (fewer than threshold NHL games)
    b2b: bool = False           # second game of a back-to-back


class TeamWeekRow(BaseModel):
    team: str
    cells: list[DayCell | None]
    games: int = 0
    offnights: int = 0
    b2b: int = 0
    free_agents: int | None = None   # healthy free agents on this NHL team (streaming table)


class WeekView(BaseModel):
    start: date
    end: date
    days: list[date]
    league_games: list[int]
    off_days: list[bool]
    rows: list[TeamWeekRow]      # most games first, then off-night games
    threshold: int = OFFNIGHT_THRESHOLD

    @property
    def label(self) -> str:
        return f"{self.start:%b %d} - {self.end:%b %d}"


def _team_row(ctx: LeagueContext, team: str, days: Sequence[date], gpd: Mapping[date, int],
              threshold: int) -> TeamWeekRow:
    dates = set(ctx.schedule.get(team, []))
    opp = (ctx.opponents or {}).get(team, {})
    cells: list[DayCell | None] = []
    for d in days:
        if d in dates:
            cells.append(DayCell(date=d, opp=opp.get(d), off=is_offnight(gpd.get(d, 0), threshold),
                                 b2b=(d - timedelta(days=1)) in dates))
        else:
            cells.append(None)
    played = [c for c in cells if c is not None]
    return TeamWeekRow(team=team, cells=cells, games=len(played), offnights=sum(c.off for c in played),
                       b2b=sum(c.b2b for c in played))


def _rank_rows(rows: list[TeamWeekRow]) -> list[TeamWeekRow]:
    return sorted(rows, key=lambda r: (-r.games, -r.offnights, r.b2b, r.team))


def week_rows(ctx: LeagueContext, week_start: date, threshold: int = OFFNIGHT_THRESHOLD,
              days: int = 7) -> WeekView:
    """The week starting on ``week_start``'s Monday as a team x day matrix."""
    start = monday(week_start)
    ds = [start + timedelta(days=i) for i in range(days)]
    gpd = per_day(ctx)
    rows = [_team_row(ctx, t, ds, gpd, threshold) for t in ctx.schedule]
    return WeekView(start=start, end=ds[-1], days=ds, league_games=[gpd.get(d, 0) for d in ds],
                    off_days=[is_offnight(gpd.get(d, 0), threshold) for d in ds], rows=_rank_rows(rows),
                    threshold=threshold)


# --------------------------------------------------------------------------- streaming

def _group(p: Player) -> str:
    if p.is_goalie:
        return "G"
    if "D" in p.positions and not set(p.positions) & {"C", "LW", "RW", "F"}:
        return "D"
    return "F"


def _num(v: Any) -> float | None:
    return float(v) if isinstance(v, (int, float)) and not isinstance(v, bool) else None


def offnight_bonus() -> float:
    try:
        from ..valuation.params import offnight_bonus as ob

        return float(ob())
    except Exception:  # noqa: BLE001 - params are optional here
        return 0.05


def per_game_value(pv: Any) -> float | None:
    """Expected fantasy points per NHL team game over the coming days: the week projection
    divided by its games and off-night multiplier (availability and goalie start share included),
    else the healthy FPG (times the start share for goalies)."""
    if pv is None:
        return None
    proj, games = _num(getattr(pv, "proj_week", None)), getattr(pv, "games_next7", None)
    if proj is not None and isinstance(games, int) and games > 0:
        off = getattr(pv, "offnight_next7", None) or 0
        return proj / (games * (1 + offnight_bonus() * off))
    fpg = _num(getattr(pv, "fpg", None))
    if fpg is None:
        return None
    share = _num(getattr(pv, "start_share", None))
    return fpg * (share if share is not None else 1.0)


class StreamTarget(BaseModel):
    cid: str
    name: str
    team: str | None
    pos: str
    group: str
    status: str
    games: int
    offnights: int
    b2b: int
    per_game: float
    proj: float
    pct_owned: float | None = None
    opps: list[str] = Field(default_factory=list)


class StreamingPlan(BaseModel):
    week_start: date
    week_end: date
    from_day: date                    # first day counted (today once the week has started)
    by_slot: dict[str, list[StreamTarget]] = Field(default_factory=dict)
    teams: list[TeamWeekRow] = Field(default_factory=list)   # best NHL teams to stream from
    bonus: float = 0.05


def streaming_targets(ctx: LeagueContext, values: Mapping[str, Any], week_start: date,
                      slots: Sequence[str] = ("F", "D", "G"), limit: int = 10, today: date | None = None,
                      team_limit: int = 8, end: date | None = None,
                      threshold: int = OFFNIGHT_THRESHOLD) -> StreamingPlan:
    """Free agents ranked by projected points over the rest of the week starting at
    ``week_start``'s Monday (from ``today`` = ``ctx.as_of`` once the week has begun):
    per-game value x games x (1 + bonus x off-night games). Injured / suspended free agents are
    skipped. ``teams`` ranks the NHL teams with the most games left (then off-night games)."""
    start = monday(week_start)
    week_end = end or (start + timedelta(days=6))
    today = today or ctx.as_of
    from_day = max(start, today) if today <= week_end else start
    gpd = per_day(ctx)
    bonus = offnight_bonus()
    days = [from_day + timedelta(days=i) for i in range((week_end - from_day).days + 1)]
    fa_by_team: dict[str, int] = {}
    by_slot: dict[str, list[StreamTarget]] = {s: [] for s in slots}
    for p in ctx.free_agents:
        if p.status in UNAVAILABLE:
            continue
        g = _group(p)
        if p.team:
            fa_by_team[p.team] = fa_by_team.get(p.team, 0) + 1
        if g not in by_slot or not p.team or p.team not in ctx.schedule:
            continue
        pg = per_game_value(values.get(p.cid))
        if pg is None:
            continue
        games, off, b2b = window_counts(ctx.schedule[p.team], gpd, from_day, week_end, threshold)
        if games == 0:
            continue
        opp = (ctx.opponents or {}).get(p.team, {})
        opps = [opp.get(d) or d.strftime("%a") for d in days if d in set(ctx.schedule[p.team])]
        by_slot[g].append(StreamTarget(
            cid=p.cid, name=p.name, team=p.team, pos="/".join(x for x in p.positions if x != "F") or g,
            group=g, status=p.status, games=games, offnights=off, b2b=b2b, per_game=pg,
            proj=pg * games * (1 + bonus * off), pct_owned=p.pct_owned, opps=opps))
    for s in by_slot:
        by_slot[s] = sorted(by_slot[s], key=lambda t: (-t.proj, t.name))[:limit]
    rows = [_team_row(ctx, t, days, gpd, threshold) for t in ctx.schedule] if days else []
    for r in rows:
        r.free_agents = fa_by_team.get(r.team, 0)
    return StreamingPlan(week_start=start, week_end=week_end, from_day=from_day, by_slot=by_slot,
                         teams=_rank_rows(rows)[:team_limit], bonus=bonus)


# --------------------------------------------------------------------------- league calendar

class Period(BaseModel):
    number: int
    start: date
    end: date
    playoffs: bool = False

    def contains(self, d: date) -> bool:
        return self.start <= d <= self.end

    @property
    def days(self) -> int:
        return (self.end - self.start).days + 1

    @property
    def label(self) -> str:
        return f"{self.start:%b %d} - {self.end:%b %d}"


class PeriodCalendar(BaseModel):
    periods: list[Period] = Field(default_factory=list)
    playoff_start: int | None = None
    source: str = "calendar weeks"
    notes: list[str] = Field(default_factory=list)

    def current(self, today: date) -> Period | None:
        """The period containing ``today``, else the next one, else the last."""
        for p in self.periods:
            if p.contains(today):
                return p
        upcoming = [p for p in self.periods if p.start > today]
        if upcoming:
            return upcoming[0]
        return self.periods[-1] if self.periods else None

    def get(self, number: int) -> Period | None:
        return next((p for p in self.periods if p.number == number), None)

    @property
    def playoff_periods(self) -> list[Period]:
        return [p for p in self.periods if p.playoffs]


def _mark_playoffs(periods: list[Period], playoff_start: int | None) -> list[Period]:
    for p in periods:
        p.playoffs = playoff_start is not None and p.number >= playoff_start
    return periods


def calendar_weeks(ctx: LeagueContext, n_periods: int | None = None, first: date | None = None,
                   last: date | None = None) -> list[tuple[date, date]]:
    """Monday-Sunday weeks covering [first, last] (default: the loaded regular season). A light
    week (fewer than 40% of a normal week's NHL games: the All-Star break) is merged into the week
    after it, as Fantrax does; while there are still more weeks than ``n_periods`` the first two
    are merged (a short opening week joins week 2)."""
    bounds = season_bounds(ctx)
    first = first or (bounds[0] if bounds else None)
    last = last or (bounds[1] if bounds else None)
    if first is None or last is None or last < first:
        return []
    gpd = per_day(ctx)
    weeks: list[list[date]] = []
    w = monday(first)
    while w <= last:
        weeks.append([max(w, first), min(w + timedelta(days=6), last)])
        w += timedelta(days=7)
    if gpd and len(weeks) > 1:
        counts = [sum(gpd.get(a + timedelta(days=i), 0) for i in range((b - a).days + 1)) for a, b in weeks]
        full = sorted(c for (a, b), c in zip(weeks, counts) if (b - a).days == 6)
        median = full[len(full) // 2] if full else 0
        merged: list[list[date]] = []
        carry: date | None = None
        for i, ((a, b), c) in enumerate(zip(weeks, counts)):
            light = (b - a).days == 6 and c < LIGHT_WEEK_SHARE * median
            if light and i + 1 < len(weeks):
                carry = carry or a          # a break week joins the week after it
                continue
            if light and merged:
                merged[-1][1] = b
                continue
            merged.append([carry or a, b])
            carry = None
        weeks = merged
    while n_periods and len(weeks) > n_periods and len(weeks) > 1:
        weeks[1][0] = weeks[0][0]
        weeks.pop(0)
    return [(a, b) for a, b in weeks]


def parse_fantrax_period(name: str) -> tuple[date, date] | None:
    """"(Mar 22/27 - Mar 28/27)" -> (2027-03-22, 2027-03-28)."""
    m = _FX_PERIOD_RE.search(name or "")
    if not m:
        return None
    try:
        return (datetime.strptime(m.group(1), "%b %d/%y").date(),
                datetime.strptime(m.group(2), "%b %d/%y").date())
    except ValueError:
        return None


def fantrax_periods(scoring_period_list: Iterable[Mapping[str, Any]]) -> list[Period]:
    """Fantrax ``displayedLists.scoringPeriodList`` -> periods ("Full Season" is skipped)."""
    out = []
    for item in scoring_period_list or []:
        try:
            num = int(item.get("value"))
        except (TypeError, ValueError):
            continue
        rng = parse_fantrax_period(str(item.get("name") or ""))
        if rng is None or num >= 9999:
            continue
        out.append(Period(number=num, start=rng[0], end=rng[1]))
    return sorted(out, key=lambda p: p.number)


def _env_playoff_start() -> int | None:
    raw = os.environ.get(PLAYOFF_ENV, "").strip()
    try:
        return int(raw) if raw else None
    except ValueError:
        return None


def _fantrax_playoff_start(provider: Any) -> tuple[int | None, str]:
    rules = getattr(provider, "rules", None)
    if rules is not None:
        try:
            v = rules.get_int(FANTRAX_PLAYOFF_LABEL)
            if v:
                return v, "Fantrax Rules page"
        except Exception:  # noqa: BLE001
            pass
    raw_fn = getattr(provider, "raw_settings", None)
    if callable(raw_fn):
        try:
            raw = raw_fn() or {}
            for sect in (raw.get("league_rules") or {}).values():
                for k, v in (sect or {}).items():
                    if "playoffs will begin" in str(k).lower():
                        m = re.match(r"\s*(\d+)", str(v))
                        if m:
                            return int(m.group(1)), "Fantrax Rules page"
        except Exception:  # noqa: BLE001
            pass
    env = _env_playoff_start()
    if env:
        return env, PLAYOFF_ENV
    return DEFAULT_PLAYOFF_START.get("fantrax"), "default for this league (period 25)"


def fantrax_calendar(ctx: LeagueContext, provider: Any) -> PeriodCalendar:
    """Scoring periods from ``getTeamRosterInfo`` (view GAMES_PER_POS) via the provider's
    FxpaClient, playoff start from the Rules page."""
    client = provider._get_client()
    data = client.call(("getTeamRosterInfo", {"view": "GAMES_PER_POS"}))[0]
    periods = fantrax_periods(((data or {}).get("displayedLists") or {}).get("scoringPeriodList") or [])
    if not periods:
        raise ValueError("Fantrax returned no scoring periods")
    start, src = _fantrax_playoff_start(provider)
    return PeriodCalendar(periods=_mark_playoffs(periods, start), playoff_start=start,
                          source="Fantrax scoring periods", notes=[f"Playoffs start period {start} ({src})"])


def espn_league(provider: Any) -> Any:
    """The provider's espn_api League (cached on the provider when it offers that)."""
    fn = getattr(provider, "_league_cached", None) or getattr(provider, "_league")
    return fn()


def espn_first_day(ctx: LeagueContext, league: Any) -> date | None:
    """Date of ESPN scoring period 1 (opening night): from today's scoring period once the season
    is under way, else the NHL opening date."""
    try:
        cur = int(getattr(league, "current_week", 0) or 0)
    except (TypeError, ValueError):
        cur = 0
    if cur > 1 and (ctx.season_start is None or ctx.as_of >= ctx.season_start):
        return ctx.as_of - timedelta(days=cur - 1)
    if ctx.season_start:
        return ctx.season_start
    bounds = season_bounds(ctx)
    return bounds[0] if bounds else None


def espn_calendar(ctx: LeagueContext, provider: Any, league: Any = None) -> PeriodCalendar:
    """ESPN matchup periods (``settings.matchup_periods``, one calendar week each) mapped to dates:
    daily scoring periods run from opening night to ``finalScoringPeriod``; weeks are Monday-Sunday
    with the All-Star break week merged into the next one (and the opening week into week 2 if
    there are still too many) until the count matches. Playoffs start after
    ``settings.reg_season_count``. Dates are derived, not read from ESPN."""
    league = league if league is not None else espn_league(provider)
    st = league.settings
    n = len(getattr(st, "matchup_periods", None) or {}) or None
    first = espn_first_day(ctx, league)
    if first is None:
        raise ValueError("no NHL opening date to anchor ESPN scoring periods")
    final = getattr(league, "finalScoringPeriod", None)
    last = first + timedelta(days=int(final) - 1) if isinstance(final, int) and final > 0 else None
    weeks = calendar_weeks(ctx, n, first, last)
    periods = [Period(number=i + 1, start=a, end=b) for i, (a, b) in enumerate(weeks)]
    reg = getattr(st, "reg_season_count", None)
    start = int(reg) + 1 if isinstance(reg, int) and n and reg < n else None
    notes = [f"ESPN: {n or len(periods)} matchup periods, playoffs start period {start}"
             if start else f"ESPN: {n or len(periods)} matchup periods"]
    if n and len(periods) != n:
        notes.append(f"ESPN period dates are approximate: {len(periods)} calendar weeks for {n} matchup periods")
    return PeriodCalendar(periods=_mark_playoffs(periods, start), playoff_start=start,
                          source="ESPN matchup periods", notes=notes)


def fallback_calendar(ctx: LeagueContext) -> PeriodCalendar:
    weeks = calendar_weeks(ctx)
    periods = [Period(number=i + 1, start=a, end=b) for i, (a, b) in enumerate(weeks)]
    start = _env_playoff_start() or DEFAULT_PLAYOFF_START.get(ctx.provider)
    how = PLAYOFF_ENV if _env_playoff_start() else "default"
    if start is None and periods:
        start = max(1, len(periods) - FALLBACK_PLAYOFF_WEEKS + 1)
        how = f"assumed: last {FALLBACK_PLAYOFF_WEEKS} weeks"
    return PeriodCalendar(periods=_mark_playoffs(periods, start), playoff_start=start,
                          source="NHL calendar weeks (Mon-Sun)",
                          notes=[f"Playoffs start period {start} ({how})"] if start else [])


def league_calendar(ctx: LeagueContext, provider: Any = None) -> PeriodCalendar:
    """The league's periods from the provider (ESPN / Fantrax), else Monday-Sunday weeks. Provider
    failures become notes, never exceptions."""
    notes: list[str] = []
    if provider is not None:
        try:
            if ctx.provider == "espn":
                return espn_calendar(ctx, provider)
            if ctx.provider == "fantrax":
                return fantrax_calendar(ctx, provider)
        except Exception as e:  # noqa: BLE001 - best effort
            notes.append(f"League periods unavailable ({type(e).__name__}: {e}); using calendar weeks")
    cal = fallback_calendar(ctx)
    cal.notes = notes + cal.notes
    return cal


# --------------------------------------------------------------------------- fantasy playoffs

class PlayoffTeam(BaseModel):
    team: str
    games: list[int]
    offnights: list[int]
    b2b: list[int]
    total: int
    total_offnights: int
    total_b2b: int


class PlayoffWeeks(BaseModel):
    periods: list[Period] = Field(default_factory=list)
    playoff_start: int | None = None
    source: str = ""
    notes: list[str] = Field(default_factory=list)
    teams: list[PlayoffTeam] = Field(default_factory=list)   # most playoff games first
    avg_total: float = 0.0


def playoff_weeks(ctx: LeagueContext, provider: Any = None, calendar: PeriodCalendar | None = None,
                  threshold: int = OFFNIGHT_THRESHOLD) -> PlayoffWeeks:
    """Fantasy-playoff periods and each NHL team's games / off-night games / back-to-backs in them,
    most games first."""
    cal = calendar or league_calendar(ctx, provider)
    periods = cal.playoff_periods
    out = PlayoffWeeks(periods=periods, playoff_start=cal.playoff_start, source=cal.source, notes=list(cal.notes))
    if not periods or not ctx.schedule:
        if not periods:
            out.notes.append("No fantasy-playoff periods found")
        return out
    gpd = per_day(ctx)
    teams = []
    for team, dates in ctx.schedule.items():
        cells = [window_counts(dates, gpd, p.start, p.end, threshold) for p in periods]
        g, o, b = ([c[i] for c in cells] for i in range(3))
        teams.append(PlayoffTeam(team=team, games=g, offnights=o, b2b=b, total=sum(g), total_offnights=sum(o),
                                 total_b2b=sum(b)))
    teams.sort(key=lambda t: (-t.total, -t.total_offnights, t.total_b2b, t.team))
    out.teams = teams
    out.avg_total = sum(t.total for t in teams) / len(teams)
    return out
