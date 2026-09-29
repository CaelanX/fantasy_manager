"""Schedule factor for the week horizon.

``ProjWeek = FPG * avail_week * games_next7 * (1 + 0.05 * offnight_games)``; goalies are also
multiplied by a shrunk start share ``(GS + k*0.5) / (team GP + k)`` with k = 10 (the same
share scales goalie season values in valuate.py).

Before the regular season (no NHL games in the window starting at ``as_of`` and
``season_start`` in the future) the window is the first 7 days from ``season_start``.
"""
from __future__ import annotations

from datetime import date, timedelta
from typing import Iterable, Mapping

from pydantic import BaseModel

from ..matching.normalize import normalize_team
from ..models import LeagueContext, Player

WEEK_DAYS = 7
OFFNIGHT_THRESHOLD = 8
OFFNIGHT_BONUS = 0.05
START_SHARE_K = 10
START_SHARE_PRIOR = 0.5


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


def proj_week(fpg: float, avail_week: float, games_next7: int, offnight_games: int) -> float:
    return fpg * avail_week * games_next7 * (1 + OFFNIGHT_BONUS * offnight_games)


def start_share(player: Player, k: int = START_SHARE_K, prior: float = START_SHARE_PRIOR,
                team_games: Mapping[str, float] | None = None) -> float:
    """Shrunk goalie start share (1.0 for skaters): ``(GS + k*prior) / (team GP + k)``.

    Starts and games are pooled over the season and prior lines (the projected line when
    neither carries games). GS falls back to the goalie's GP when a line has no GS column.
    ``team_games`` ({split: team games played}) gives the denominator per split; without it
    (or for a split it does not cover) the goalie's own GP is used, i.e. GS/GP. The
    denominator is never below the goalie's own GP."""
    if not player.is_goalie:
        return 1.0
    return start_share_parts(player, k, prior, team_games)[0]


def start_share_parts(player: Player, k: int = START_SHARE_K, prior: float = START_SHARE_PRIOR,
                      team_games: Mapping[str, float] | None = None) -> tuple[float, float, float]:
    """(share, starts, team games) behind :func:`start_share`."""
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

    gs, tg = pooled(("season", "prior"))
    if tg <= 0:
        gs, tg = pooled(("projected",))
    return (gs + k * prior) / (tg + k), gs, tg


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
        return self.games * (1 + OFFNIGHT_BONUS * self.offnight) * self.start_share


def schedule_factor(player: Player, schedule: Mapping[str, list[date]], games_per_day: Mapping[date, int],
                    window: WeekWindow, threshold: int = OFFNIGHT_THRESHOLD,
                    share: float | None = None) -> ScheduleFactor | None:
    """Games / off-night games / start share for one player's NHL team, or None if unknown team."""
    team = normalize_team(player.team)
    if not team or team not in schedule:
        return None
    dates = schedule[team]
    return ScheduleFactor(games=games_in_next(dates, window.start, window.days),
                          offnight=off_night_games(dates, games_per_day, window.start, window.days, threshold),
                          start_share=start_share(player) if share is None else share,
                          window=window)
