"""Schedule factor for the week horizon.

``ProjWeek = FPG * avail_week * games_next7 * (1 + 0.05 * offnight_games)``; goalies are also
multiplied by a shrunk start share ``(GS + k*0.5) / (team GP + k)`` with k = 10 (the same
share scales goalie season values in valuate.py).

Before the regular season (no NHL games in the window starting at ``as_of`` and
``season_start`` in the future) the window is the first 7 days from ``season_start``.

The off-night bonus and the start-share prior / k are read from ``params`` at call time (a
harness version may override them); the module constants are import-time snapshots.

Actual starts (``harness.deployment`` ledger, opt-in): ``start_share(..., actual=(starts, team
games))`` shrinks this season's per-game starts / team games toward the projection-based or
prior share with k = ``ACTUAL_SHARE_K`` (10): ``(starts + 10*base) / (team games + 10)``, where
``base`` is the share from the prior (else projected) line only, so this season is not counted
twice. ``actual_start_share`` does the same for a base share computed elsewhere.

Back-to-backs (opt-in, week horizon): ``week_start_share`` / ``schedule_factor(b2b_p=...)``
re-weight a goalie's share for the second nights of back-to-backs in the window. The #1 (share
>= 0.5) starts a second night with probability ``b2b_p`` (0.35 until the ledger has 5 of the
team's back-to-backs, then ``harness.deployment.b2b_second_night_rate``); a backup (share
0.25-0.5) with ``1 - b2b_p``. The other nights get the share that keeps the season average
unchanged given ``SEASON_B2B_FRAC`` of games are second nights.
"""
from __future__ import annotations

from datetime import date, timedelta
from typing import Any, Iterable, Mapping

from pydantic import BaseModel

from ..matching.normalize import normalize_team
from ..models import LeagueContext, Player
from . import params as _params

WEEK_DAYS = 7
OFFNIGHT_THRESHOLD = 8
# import-time snapshots (read-only aliases; live code uses the params accessors)
OFFNIGHT_BONUS = _params.offnight_bonus()
START_SHARE_K = _params.start_share_k()
START_SHARE_PRIOR = _params.start_share_prior()
# actual starts (harness ledger) shrunk toward the projection / prior share
ACTUAL_SHARE_K = 10.0
# goalie back-to-backs: P(#1 starts the second night) until the ledger has B2B_MIN_SAMPLE of the
# team's back-to-backs (then the observed rate shrunk toward it with k = B2B_SHRINK_K)
B2B_SECOND_NIGHT_P = 0.35
B2B_MIN_SAMPLE = 5
B2B_SHRINK_K = 5.0
SEASON_B2B_FRAC = 0.15      # share of a team's games that are the second night of a back-to-back
STARTER_SHARE = 0.5
BACKUP_MIN_SHARE = 0.25


def _in_window(d: date, start: date, days: int) -> bool:
    return start <= d < start + timedelta(days=days)


def games_in_next(dates: Iterable[date], as_of: date, days: int = WEEK_DAYS) -> int:
    """Games on dates in [as_of, as_of + days)."""
    return sum(1 for d in set(dates) if _in_window(d, as_of, days))


def off_night_games(dates: Iterable[date], games_per_day: Mapping[date, int], as_of: date,
                    days: int = WEEK_DAYS, threshold: int = OFFNIGHT_THRESHOLD) -> int:
    """A team's games in the window played on nights with fewer than ``threshold`` NHL games."""
    return sum(1 for d in set(dates)
               if _in_window(d, as_of, days) and 0 < games_per_day.get(d, 0) < threshold)


def proj_week(fpg: float, avail_week: float, games_next7: int, offnight_games: int,
              params: Mapping[str, Any] | None = None) -> float:
    return fpg * avail_week * games_next7 * (1 + _params.offnight_bonus(params) * offnight_games)


def shrunk_share(starts: float, team_games: float, k: float | None = None, prior: float | None = None,
                 params: Mapping[str, Any] | None = None) -> float:
    """``(GS + k*prior) / (team GP + k)`` with k / prior from ``params`` unless given."""
    k = _params.start_share_k(params) if k is None else k
    prior = _params.start_share_prior(params) if prior is None else prior
    return (starts + k * prior) / (team_games + k)


def start_share(player: Player, k: float | None = None, prior: float | None = None,
                team_games: Mapping[str, float] | None = None,
                actual: tuple[float, float] | None = None, actual_k: float = ACTUAL_SHARE_K) -> float:
    """Shrunk goalie start share (1.0 for skaters): ``(GS + k*prior) / (team GP + k)``.

    Starts and games are pooled over the season and prior lines (the projected line when
    neither carries games). GS falls back to the goalie's GP when a line has no GS column.
    ``team_games`` ({split: team games played}) gives the denominator per split; without it
    (or for a split it does not cover) the goalie's own GP is used, i.e. GS/GP. The
    denominator is never below the goalie's own GP.

    ``actual`` = (starts, team games) this season from per-game goalie starts (the harness
    ledger): the result is ``(starts + actual_k*base) / (team games + actual_k)`` with ``base``
    the shrunk share of the prior line (else the projected line), leaving the season line out
    because ``actual`` already counts this season."""
    if not player.is_goalie:
        return 1.0
    if actual is not None and actual[1] > 0:
        base = start_share_parts(player, k, prior, team_games, splits=("prior",))[0]
        return actual_start_share(base, actual[0], actual[1], actual_k)
    return start_share_parts(player, k, prior, team_games)[0]


def actual_start_share(base: float, starts: float, team_games: float, k: float = ACTUAL_SHARE_K) -> float:
    """This season's per-game starts / team games shrunk toward ``base`` (the projection-based
    or prior share) with ``k`` games: ``(starts + k*base) / (team games + k)``."""
    if team_games <= 0:
        return base
    starts = min(max(float(starts), 0.0), float(team_games))
    return (starts + k * base) / (float(team_games) + k)


def start_share_parts(player: Player, k: float | None = None, prior: float | None = None,
                      team_games: Mapping[str, float] | None = None,
                      splits: tuple[str, ...] = ("season", "prior")) -> tuple[float, float, float]:
    """(share, starts, team games) behind :func:`start_share` (pooled over ``splits``; the
    projected line when those carry no games)."""
    def pooled(splits: tuple[str, ...]) -> tuple[float, float]:
        gs = tg = 0.0
        for split in splits:
            line = player.lines.get(split)
            if line is None:
                continue
            starts = float(line.stats.get("GS", line.gp))
            own = float(max(line.gp, starts))
            denom = max(float((team_games or {}).get(split, own)), own)
            if denom <= 0:
                continue
            gs += starts
            tg += denom
        return gs, tg

    gs, tg = pooled(splits)
    if tg <= 0:
        gs, tg = pooled(("projected",))
    return shrunk_share(gs, tg, k, prior), gs, tg


class WeekWindow(BaseModel):
    start: date
    days: int = WEEK_DAYS
    preseason: bool = False
    avg_team_games: float = 0.0   # mean games per NHL team in the window

    @property
    def end(self) -> date:
        return self.start + timedelta(days=self.days - 1)

    def label(self) -> str:
        return f"{self.start:%b %d}-{self.end:%b %d}"


def week_window(as_of: date, season_start: date | None, schedule: Mapping[str, list[date]],
                games_per_day: Mapping[date, int], days: int = WEEK_DAYS) -> WeekWindow | None:
    """Window to project over, or None when no schedule is loaded."""
    if not schedule:
        return None
    start, preseason = as_of, False
    league_games = sum(n for d, n in games_per_day.items() if _in_window(d, as_of, days))
    if league_games == 0 and not games_per_day:
        league_games = sum(games_in_next(ds, as_of, days) for ds in schedule.values())
    if league_games == 0 and season_start is not None and season_start > as_of:
        start, preseason = season_start, True
    counts = [games_in_next(ds, start, days) for ds in schedule.values()]
    avg = sum(counts) / len(counts) if counts else 0.0
    return WeekWindow(start=start, days=days, preseason=preseason, avg_team_games=avg)


def context_window(ctx: LeagueContext, days: int = WEEK_DAYS) -> WeekWindow | None:
    return week_window(ctx.as_of, ctx.season_start, ctx.schedule, ctx.games_per_day, days)


class ScheduleFactor(BaseModel):
    games: int
    offnight: int
    start_share: float = 1.0
    window: WeekWindow

    @property
    def multiplier(self) -> float:
        """Games-weighted multiplier applied to availability-adjusted FPG."""
        return self.games * (1 + _params.offnight_bonus() * self.offnight) * self.start_share


def second_nights(dates: Iterable[date], start: date, days: int = WEEK_DAYS) -> int:
    """Games in [start, start + days) played the day after another game (the previous game
    may fall before the window)."""
    ds = set(dates)
    return sum(1 for d in ds if _in_window(d, start, days) and d - timedelta(days=1) in ds)


def week_start_share(share: float, dates: Iterable[date], start: date, days: int = WEEK_DAYS,
                     b2b_p: float = B2B_SECOND_NIGHT_P, season_b2b_frac: float = SEASON_B2B_FRAC) -> float:
    """Expected fraction of the team's games in the window that the goalie starts, given his
    season ``share`` and the window's back-to-backs (see the module docstring). Unchanged for a
    third goalie (share < 0.25) or a window without games."""
    ds = set(dates)
    games = games_in_next(ds, start, days)
    if games <= 0:
        return share
    if share >= STARTER_SHARE:
        p2 = b2b_p
    elif share >= BACKUP_MIN_SHARE:
        p2 = 1.0 - b2b_p
    else:
        return share
    b = second_nights(ds, start, days)
    f = min(max(season_b2b_frac, 0.0), 0.9)
    p1 = min(1.0, max(0.0, (share - f * p2) / (1.0 - f)))
    return ((games - b) * p1 + b * p2) / games


def schedule_factor(player: Player, schedule: Mapping[str, list[date]], games_per_day: Mapping[date, int],
                    window: WeekWindow, threshold: int = OFFNIGHT_THRESHOLD,
                    share: float | None = None, b2b_p: float | None = None) -> ScheduleFactor | None:
    """Games / off-night games / start share for one player's NHL team, or None if unknown team.

    ``b2b_p`` (goalies, opt-in): P(#1 starts a back-to-back's second night) for
    :func:`week_start_share`; None keeps the season share."""
    team = normalize_team(player.team)
    if not team or team not in schedule:
        return None
    dates = schedule[team]
    s = start_share(player) if share is None else share
    if b2b_p is not None and player.is_goalie:
        s = week_start_share(s, dates, window.start, window.days, b2b_p)
    return ScheduleFactor(games=games_in_next(dates, window.start, window.days),
                          offnight=off_night_games(dates, games_per_day, window.start, window.days, threshold),
                          start_share=s,
                          window=window)
