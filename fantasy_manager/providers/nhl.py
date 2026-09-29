"""NHL data provider: season stats (stats REST API) and schedules, rosters, game logs,
player pages and search (api-web.nhle.com).

All network access goes through an injected ``fetch_json(url, params)`` callable so the
integrator can swap in a caching fetcher and tests can replay recorded fixtures.
"""

from __future__ import annotations

import json
import re
from collections import Counter
from datetime import date, datetime, timedelta
from datetime import date as Date
from typing import Any, Callable, Iterable, Literal

from pathlib import Path

import httpx
from pydantic import BaseModel, Field

FetchJson = Callable[[str, "dict | None"], Any]

STATS_BASE = "https://api.nhle.com/stats/rest/en"
WEB_BASE = "https://api-web.nhle.com/v1"
SEARCH_URL = "https://search.d3.nhle.com/api/v1/search/player"

NHL_TEAMS: tuple[str, ...] = (
    "ANA", "BOS", "BUF", "CGY", "CAR", "CHI", "COL", "CBJ", "DAL", "DET", "EDM",
    "FLA", "LAK", "MIN", "MTL", "NSH", "NJD", "NYI", "NYR", "OTT", "PHI", "PIT",
    "SJS", "SEA", "STL", "TBL", "TOR", "UTA", "VAN", "VGK", "WSH", "WPG",
)

Position = Literal["C", "LW", "RW", "D", "G"]
_POSITION_MAP: dict[str, str] = {"C": "C", "L": "LW", "LW": "LW", "R": "RW", "RW": "RW", "D": "D", "G": "G"}

# Canonical skater stat keys produced by this module.
SKATER_STAT_KEYS = ("GP", "G", "A", "PTS", "PM", "PIM", "PPG", "PPP", "SHG", "SHP", "GWG", "SOG", "HIT", "BLK")
GOALIE_STAT_KEYS = ("GP", "GS", "W", "L", "OTL", "GA", "SA", "SV", "SO", "GAA", "SVPCT")

_SUMMARY_MAP = {
    "gamesPlayed": "GP", "goals": "G", "assists": "A", "points": "PTS", "plusMinus": "PM",
    "penaltyMinutes": "PIM", "ppGoals": "PPG", "ppPoints": "PPP", "shGoals": "SHG",
    "shPoints": "SHP", "gameWinningGoals": "GWG", "otGoals": "OTG", "shots": "SOG",
}
_REALTIME_MAP = {
    "hits": "HIT", "blockedShots": "BLK", "giveaways": "GIVE", "takeaways": "TAKE",
    "missedShots": "MISS", "emptyNetGoals": "ENG", "emptyNetPoints": "ENP",
}
_FACEOFF_MAP = {"totalFaceoffWins": "FOW", "totalFaceoffLosses": "FOL"}
_GOALIE_MAP = {
    "gamesPlayed": "GP", "gamesStarted": "GS", "wins": "W", "losses": "L", "otLosses": "OTL",
    "goalsAgainst": "GA", "shotsAgainst": "SA", "saves": "SV", "shutouts": "SO",
    "goalsAgainstAverage": "GAA", "savePct": "SVPCT",
}
_GAMELOG_SKATER_MAP = {
    "goals": "G", "assists": "A", "points": "PTS", "plusMinus": "PM", "pim": "PIM",
    "powerPlayGoals": "PPG", "powerPlayPoints": "PPP", "shorthandedGoals": "SHG",
    "shorthandedPoints": "SHP", "gameWinningGoals": "GWG", "otGoals": "OTG", "shots": "SOG",
    "shifts": "SHIFTS",
}


# --------------------------------------------------------------------------- helpers

def default_fetch_json(url: str, params: dict | None = None) -> Any:
    """Plain uncached GET returning parsed JSON (follows the API's 307 redirects)."""
    resp = httpx.get(url, params=params, follow_redirects=True, timeout=20.0,
                     headers={"User-Agent": "fantasy-manager/0.1"})
    resp.raise_for_status()
    return resp.json()


def current_season(today: date | None = None) -> int:
    """Season id like 20262027. From September on, the upcoming season is current."""
    today = today or date.today()
    y = today.year if today.month >= 9 else today.year - 1
    return y * 10000 + (y + 1)


def prior_season(season: int) -> int:
    return season - 10001


def map_position(code: str | None) -> str | None:
    if not code:
        return None
    return _POSITION_MAP.get(code.strip().upper())


def parse_toi(value: str | int | float | None) -> float | None:
    """'24:49' -> 1489.0 seconds; numbers are passed through as seconds."""
    if value is None or value == "":
        return None
    if isinstance(value, (int, float)):
        return float(value)
    parts = str(value).split(":")
    try:
        if len(parts) == 2:
            return int(parts[0]) * 60 + float(parts[1])
        if len(parts) == 3:
            return int(parts[0]) * 3600 + int(parts[1]) * 60 + float(parts[2])
        return float(value)
    except ValueError:
        return None


def _default(obj: Any) -> str | None:
    """NHL localized strings look like {"default": "Connor", "fr": ...}."""
    if isinstance(obj, dict):
        return obj.get("default")
    return obj


def _parse_date(value: str | None) -> date | None:
    if not value:
        return None
    try:
        return date.fromisoformat(value[:10])
    except ValueError:
        return None


def _parse_dt(value: str | None) -> datetime | None:
    if not value:
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None


def _num(v: Any) -> float:
    return float(v) if isinstance(v, (int, float)) and not isinstance(v, bool) else 0.0


def split_teams(team_abbrevs: str | None) -> list[str]:
    """'SJS, TOR' -> ['SJS', 'TOR'] (last entry is the most recent team)."""
    if not team_abbrevs:
        return []
    return [t.strip() for t in str(team_abbrevs).split(",") if t.strip()]


# --------------------------------------------------------------------------- models

class NhlSkaterSeason(BaseModel):
    player_id: int
    name: str
    last_name: str | None = None
    team: str | None = None           # most recent team
    teams: list[str] = Field(default_factory=list)
    position: str | None = None       # C / LW / RW / D
    shoots: str | None = None
    season: int
    game_type: int = 2
    toi_per_game: float | None = None  # seconds
    shooting_pct: float | None = None
    faceoff_win_pct: float | None = None
    stats: dict[str, float] = Field(default_factory=dict)

    @property
    def games_played(self) -> int:
        return int(self.stats.get("GP", 0))

    def per_game(self) -> dict[str, float]:
        gp = self.stats.get("GP", 0)
        if not gp:
            return {k: 0.0 for k in self.stats if k != "GP"}
        return {k: v / gp for k, v in self.stats.items() if k != "GP"}


class NhlGoalieSeason(BaseModel):
    player_id: int
    name: str
    last_name: str | None = None
    team: str | None = None
    teams: list[str] = Field(default_factory=list)
    position: str = "G"
    catches: str | None = None
    season: int
    game_type: int = 2
    toi_seconds: float | None = None
    stats: dict[str, float] = Field(default_factory=dict)

    @property
    def games_played(self) -> int:
        return int(self.stats.get("GP", 0))


class NhlGameLogEntry(BaseModel):
    game_id: int | None = None
    date: Date
    team: str | None = None
    opponent: str | None = None
    home: bool | None = None
    game_type: int | None = None
    toi_seconds: float | None = None
    is_goalie: bool = False
    decision: str | None = None
    stats: dict[str, float] = Field(default_factory=dict)


class NhlGame(BaseModel):
    game_id: int
    season: int | None = None
    game_type: int
    date: Date
    start_time_utc: datetime | None = None
    away: str
    home: str
    state: str | None = None

    def teams(self) -> tuple[str, str]:
        return (self.away, self.home)


class NhlWeekSchedule(BaseModel):
    days: dict[date, list[NhlGame]] = Field(default_factory=dict)
    regular_season_start: date | None = None
    regular_season_end: date | None = None
    next_start_date: date | None = None
    previous_start_date: date | None = None

    def games_per_team(self) -> dict[str, int]:
        c: Counter[str] = Counter()
        for games in self.days.values():
            for g in games:
                c[g.away] += 1
                c[g.home] += 1
        return dict(c)


class NhlRosterPlayer(BaseModel):
    player_id: int
    first_name: str
    last_name: str
    team: str
    position: str | None = None      # C / LW / RW / D / G
    position_code: str | None = None  # raw NHL code
    birth_date: date | None = None
    sweater_number: int | None = None
    shoots_catches: str | None = None

    @property
    def name(self) -> str:
        return f"{self.first_name} {self.last_name}".strip()

    def age_on(self, on: date | None = None) -> float | None:
        """Age in fractional years on the given date (default today)."""
        if not self.birth_date:
            return None
        on = on or date.today()
        years = on.year - self.birth_date.year - ((on.month, on.day) < (self.birth_date.month, self.birth_date.day))
        last_bday_year = self.birth_date.year + years
        try:
            last_bday = self.birth_date.replace(year=last_bday_year)
        except ValueError:  # Feb 29
            last_bday = date(last_bday_year, 2, 28)
        return years + (on - last_bday).days / 365.25


class NhlPlayerLanding(BaseModel):
    player_id: int
    first_name: str
    last_name: str
    team: str | None = None
    position: str | None = None
    birth_date: date | None = None
    shoots_catches: str | None = None
    is_active: bool | None = None
    last5: list[NhlGameLogEntry] = Field(default_factory=list)
    featured_regular_season: dict[str, Any] = Field(default_factory=dict)
    season_totals: list[dict[str, Any]] = Field(default_factory=list)
    draft_year: int | None = None
    draft_round: int | None = None
    draft_overall: int | None = None
    draft_team: str | None = None
    career_gp: int | None = None      # careerTotals.regularSeason.gamesPlayed (0 when absent)

    @property
    def name(self) -> str:
        return f"{self.first_name} {self.last_name}".strip()


class NhlSearchResult(BaseModel):
    player_id: int
    name: str
    position: str | None = None
    position_code: str | None = None
    team: str | None = None
    active: bool | None = None
    sweater_number: int | None = None
    birth_country: str | None = None


# --------------------------------------------------------------------------- row parsers

def _skater_from_rows(summary: dict, realtime: dict | None = None, faceoffs: dict | None = None,
                      game_type: int = 2) -> NhlSkaterSeason:
    stats = {canon: _num(summary.get(src)) for src, canon in _SUMMARY_MAP.items()}
    if realtime:
        stats.update({canon: _num(realtime.get(src)) for src, canon in _REALTIME_MAP.items()})
    if faceoffs:
        stats.update({canon: _num(faceoffs.get(src)) for src, canon in _FACEOFF_MAP.items()})
    teams = split_teams(summary.get("teamAbbrevs"))
    return NhlSkaterSeason(
        player_id=int(summary["playerId"]),
        name=summary.get("skaterFullName") or "",
        last_name=summary.get("lastName"),
        team=teams[-1] if teams else None,
        teams=teams,
        position=map_position(summary.get("positionCode")),
        shoots=summary.get("shootsCatches"),
        season=int(summary.get("seasonId") or 0),
        game_type=game_type,
        toi_per_game=summary.get("timeOnIcePerGame"),
        shooting_pct=summary.get("shootingPct"),
        faceoff_win_pct=summary.get("faceoffWinPct"),
        stats=stats,
    )


def _goalie_from_row(row: dict, game_type: int = 2) -> NhlGoalieSeason:
    stats = {canon: _num(row.get(src)) for src, canon in _GOALIE_MAP.items()}
    teams = split_teams(row.get("teamAbbrevs"))
    return NhlGoalieSeason(
        player_id=int(row["playerId"]),
        name=row.get("goalieFullName") or "",
        last_name=row.get("lastName"),
        team=teams[-1] if teams else None,
        teams=teams,
        catches=row.get("shootsCatches"),
        season=int(row.get("seasonId") or 0),
        game_type=game_type,
        toi_seconds=row.get("timeOnIce"),
        stats=stats,
    )


def _game_log_entry(row: dict) -> NhlGameLogEntry:
    is_goalie = "shotsAgainst" in row or "savePctg" in row or "decision" in row
    flag = row.get("homeRoadFlag")
    stats: dict[str, float]
    if is_goalie:
        sa = _num(row.get("shotsAgainst"))
        ga = _num(row.get("goalsAgainst"))
        decision = row.get("decision")
        stats = {
            "GP": 1.0,
            "GS": _num(row.get("gamesStarted")),
            "W": 1.0 if decision == "W" else 0.0,
            "L": 1.0 if decision == "L" else 0.0,
            "OTL": 1.0 if decision in ("O", "OT", "OTL", "SO") else 0.0,
            "GA": ga,
            "SA": sa,
            "SV": sa - ga,
            "SO": _num(row.get("shutouts")),
            "SVPCT": float(row["savePctg"]) if row.get("savePctg") is not None else ((sa - ga) / sa if sa else 0.0),
            "G": _num(row.get("goals")),
            "A": _num(row.get("assists")),
            "PIM": _num(row.get("pim")),
        }
    else:
        decision = None
        stats = {"GP": 1.0}
        stats.update({canon: _num(row.get(src)) for src, canon in _GAMELOG_SKATER_MAP.items() if src in row})
    return NhlGameLogEntry(
        game_id=row.get("gameId"),
        date=_parse_date(row.get("gameDate")),
        team=row.get("teamAbbrev"),
        opponent=row.get("opponentAbbrev"),
        home=(flag == "H") if flag in ("H", "R") else None,
        game_type=row.get("gameTypeId"),
        toi_seconds=parse_toi(row.get("toi")),
        is_goalie=is_goalie,
        decision=decision,
        stats=stats,
    )


def _game_from_row(row: dict, day: date | None = None) -> NhlGame:
    return NhlGame(
        game_id=int(row["id"]),
        season=row.get("season"),
        game_type=int(row.get("gameType") or 0),
        date=_parse_date(row.get("gameDate")) or day or _parse_dt(row.get("startTimeUTC")).date(),
        start_time_utc=_parse_dt(row.get("startTimeUTC")),
        away=row["awayTeam"]["abbrev"],
        home=row["homeTeam"]["abbrev"],
        state=row.get("gameState"),
    )


# --------------------------------------------------------------------------- schedule helpers

def games_in_window(dates: Iterable[date], start: date, end: date) -> int:
    """Number of game dates in the inclusive window [start, end]."""
    return sum(1 for d in dates if start <= d <= end)


def games_per_day(all_schedules: dict[str, list[date]]) -> dict[date, int]:
    """Number of NHL games on each date given team -> game dates (each game counted once)."""
    teams_playing: Counter[date] = Counter()
    for dates in all_schedules.values():
        for d in set(dates):
            teams_playing[d] += 1
    return {d: (n + 1) // 2 for d, n in sorted(teams_playing.items())}


def off_nights(per_day: dict[date, int], threshold: int = 8) -> set[date]:
    """Dates with fewer than ``threshold`` games (more free agents / bench players available)."""
    return {d for d, n in per_day.items() if 0 < n < threshold}


def back_to_backs(dates: Iterable[date]) -> list[tuple[date, date]]:
    """Consecutive-day game pairs (relevant for goalie starts)."""
    ds = sorted(set(dates))
    return [(a, b) for a, b in zip(ds, ds[1:]) if (b - a) == timedelta(days=1)]


# --------------------------------------------------------------------------- deployment (per-game reports)
#
# Per-game rows of the stats REST reports (``isGame=true``) over a date window. Field names
# verified 2026-09-29 against the 2025-26 regular season:
#   skater/timeonice : playerId, gameId, gameDate, teamAbbrev, opponentTeamAbbrev, homeRoad,
#                      timeOnIce, evTimeOnIce, ppTimeOnIce, shTimeOnIce, otTimeOnIce (seconds), shifts
#   skater/powerplay : ... ppTimeOnIce, ppTimeOnIcePctPerGame (share of the team's PP time,
#                      0..1), ppGoals, ppPoints, ppShots
#   goalie/summary   : ... gamesStarted, shotsAgainst, saves, goalsAgainst, timeOnIce, wins,
#                      losses, otLosses
#   team/powerplay   : teamId, teamFullName (no abbreviation: it is taken from the other team's
#                      opponentTeamAbbrev in the same game), ppTimeOnIcePerGame, ppOpportunities,
#                      powerPlayGoalsFor
# A team's PP time in a game equals the sum of its skaters' ppTimeOnIce / 5 (checked on every
# game of 2026-04-01); that is the fallback when the team report lacks a game.
# Season reports (``isGame=false``) carry the same player fields with ``teamAbbrevs``.

HOUR_S = 3600.0
TTL_GAME_WINDOW_OPEN = 12 * HOUR_S          # window reaching today: games may still be in progress
TTL_GAME_WINDOW_CLOSED = 30 * 24 * HOUR_S   # finished days do not change
TTL_SEASON_REPORT = 6 * HOUR_S
_GAME_END_RE = re.compile(r'gameDate<="?(\d{4}-\d{2}-\d{2})')


def game_window_ttl(params: dict | None, today: date | None = None) -> float | None:
    """TTL for a per-game (``isGame=true``) date-window request: 12h when the window reaches
    ``today`` (or has no end), 30 days for past days; None for any other request."""
    params = params or {}
    if str(params.get("isGame", "")).lower() != "true":
        return None
    m = _GAME_END_RE.search(str(params.get("cayenneExp", "")))
    if not m:
        return TTL_GAME_WINDOW_OPEN
    end = date.fromisoformat(m.group(1))
    return TTL_GAME_WINDOW_CLOSED if end < (today or date.today()) else TTL_GAME_WINDOW_OPEN


class CachedNhlFetch:
    """``fetch_json`` for NhlClient through an ``HttpCache``-like object (``get_json(url,
    params=, ttl=)``): per-game windows use :func:`game_window_ttl`; everything else
    ``ttl_for(url, params) -> (label, ttl)`` when given (e.g. ``providers.enrich.ttl_for``),
    else ``default_ttl``."""

    def __init__(self, cache: Any, today: date | None = None, default_ttl: float = TTL_SEASON_REPORT,
                 ttl_for: Callable[[str, "dict | None"], tuple[str, float]] | None = None):
        self.cache, self.today, self.default_ttl, self.ttl_for = cache, today, default_ttl, ttl_for

    def ttl(self, url: str, params: dict | None) -> float:
        t = game_window_ttl(params, self.today)
        if t is not None:
            return t
        if self.ttl_for is not None:
            return float(self.ttl_for(url, params)[1])
        return self.default_ttl

    def __call__(self, url: str, params: dict | None = None) -> Any:
        return self.cache.get_json(url, params=params, ttl=self.ttl(url, params))


def _secs(v: Any) -> float | None:
    if isinstance(v, bool):
        return None
    return float(v) if isinstance(v, (int, float)) else parse_toi(v)


def _home(flag: Any) -> bool | None:
    return (flag == "H") if flag in ("H", "R") else None


class NhlSkaterToiGame(BaseModel):
    """One skater's ice time in one game (seconds)."""
    player_id: int
    game_id: int | None = None
    date: Date
    team: str | None = None
    opponent: str | None = None
    home: bool | None = None
    name: str | None = None
    position: str | None = None
    toi: float | None = None
    ev_toi: float | None = None
    pp_toi: float | None = None
    sh_toi: float | None = None
    ot_toi: float | None = None
    shifts: int | None = None


class NhlSkaterPpGame(BaseModel):
    """One skater's power play in one game; ``pp_share`` = NHL ppTimeOnIcePctPerGame (0..1)."""
    player_id: int
    game_id: int | None = None
    date: Date
    team: str | None = None
    opponent: str | None = None
    pp_toi: float | None = None          # seconds
    pp_share: float | None = None
    pp_goals: float = 0.0
    pp_points: float = 0.0
    pp_shots: float = 0.0


class NhlGoalieGame(BaseModel):
    player_id: int
    game_id: int | None = None
    date: Date
    team: str | None = None
    opponent: str | None = None
    home: bool | None = None
    name: str | None = None
    started: bool = False
    sa: float = 0.0
    sv: float = 0.0
    ga: float = 0.0
    toi: float | None = None             # seconds
    decision: str | None = None          # W / L / OTL


class NhlTeamPpGame(BaseModel):
    team: str | None = None              # abbreviation (from the opponent's row of the same game)
    team_id: int | None = None
    team_name: str | None = None
    game_id: int | None = None
    date: Date
    opponent: str | None = None
    pp_toi: float | None = None          # seconds of power play in the game
    pp_opportunities: float = 0.0
    pp_goals: float = 0.0


class NhlSkaterDeploymentSeason(BaseModel):
    """Season (``isGame=false``) deployment of one skater: per-game seconds and PP share."""
    player_id: int
    season: int
    name: str | None = None
    team: str | None = None
    teams: list[str] = Field(default_factory=list)
    gp: int = 0
    toi_per_game: float | None = None
    ev_toi_per_game: float | None = None
    pp_toi_per_game: float | None = None
    sh_toi_per_game: float | None = None
    pp_share: float | None = None


def _toi_game(r: dict) -> NhlSkaterToiGame:
    return NhlSkaterToiGame(
        player_id=int(r["playerId"]), game_id=r.get("gameId"), date=_parse_date(r.get("gameDate")),
        team=r.get("teamAbbrev"), opponent=r.get("opponentTeamAbbrev"), home=_home(r.get("homeRoad")),
        name=r.get("skaterFullName"), position=map_position(r.get("positionCode")),
        toi=_secs(r.get("timeOnIce")), ev_toi=_secs(r.get("evTimeOnIce")), pp_toi=_secs(r.get("ppTimeOnIce")),
        sh_toi=_secs(r.get("shTimeOnIce")), ot_toi=_secs(r.get("otTimeOnIce")),
        shifts=int(r["shifts"]) if isinstance(r.get("shifts"), (int, float)) else None)


def _pp_game(r: dict) -> NhlSkaterPpGame:
    pct = r.get("ppTimeOnIcePctPerGame")
    return NhlSkaterPpGame(
        player_id=int(r["playerId"]), game_id=r.get("gameId"), date=_parse_date(r.get("gameDate")),
        team=r.get("teamAbbrev"), opponent=r.get("opponentTeamAbbrev"), pp_toi=_secs(r.get("ppTimeOnIce")),
        pp_share=float(pct) if isinstance(pct, (int, float)) and not isinstance(pct, bool) else None,
        pp_goals=_num(r.get("ppGoals")), pp_points=_num(r.get("ppPoints")), pp_shots=_num(r.get("ppShots")))


def _goalie_game(r: dict) -> NhlGoalieGame:
    decision = ("W" if _num(r.get("wins")) else "L" if _num(r.get("losses"))
                else "OTL" if _num(r.get("otLosses")) else None)
    sa, ga = _num(r.get("shotsAgainst")), _num(r.get("goalsAgainst"))
    return NhlGoalieGame(
        player_id=int(r["playerId"]), game_id=r.get("gameId"), date=_parse_date(r.get("gameDate")),
        team=r.get("teamAbbrev"), opponent=r.get("opponentTeamAbbrev"), home=_home(r.get("homeRoad")),
        name=r.get("goalieFullName"), started=bool(_num(r.get("gamesStarted"))), sa=sa,
        sv=_num(r.get("saves")) if r.get("saves") is not None else sa - ga, ga=ga,
        toi=_secs(r.get("timeOnIce")), decision=decision)


def _team_pp_games(rows: list[dict]) -> list[NhlTeamPpGame]:
    """Team rows carry no abbreviation: take it from the other team's opponentTeamAbbrev."""
    by_game: dict[Any, list[dict]] = {}
    for r in rows:
        by_game.setdefault(r.get("gameId"), []).append(r)
    out = []
    for r in rows:
        others = [o for o in by_game.get(r.get("gameId"), []) if o.get("teamId") != r.get("teamId")]
        abbrev = r.get("teamAbbrev") or (others[0].get("opponentTeamAbbrev") if len(others) == 1 else None)
        out.append(NhlTeamPpGame(
            team=abbrev, team_id=r.get("teamId"), team_name=r.get("teamFullName"), game_id=r.get("gameId"),
            date=_parse_date(r.get("gameDate")), opponent=r.get("opponentTeamAbbrev"),
            pp_toi=_secs(r.get("ppTimeOnIcePerGame")), pp_opportunities=_num(r.get("ppOpportunities")),
            pp_goals=_num(r.get("powerPlayGoalsFor"))))
    return out


def game_window_filter(start: date | None, end: date | None) -> str | None:
    """``gameDate>="start" and gameDate<="end"`` (either side optional)."""
    parts = []
    if start is not None:
        parts.append(f'gameDate>="{start.isoformat()}"')
    if end is not None:
        parts.append(f'gameDate<="{end.isoformat()}"')
    return " and ".join(parts) or None


# --------------------------------------------------------------------------- preseason (gameType 1)
#
# Verified live 2026-09-29: the NHL publishes NO preseason data through the stats REST reports
# (``gameTypeId=1`` returns total 0 for summary / timeonice / powerplay, season and per-game,
# 2025-26 and 2026-27 alike) nor through the player game logs (``/game-log/{season}/1`` has no
# ``gameLog``). Preseason games do appear in ``club-schedule-season`` (gameType 1) and their
# ``gamecenter/{id}/boxscore`` carries per-skater G / A / PTS / +/- / PIM / hits / blocks / PPG /
# SOG / TOI / shifts; ``gamecenter/{id}/landing`` lists every goal with its strength (ev / pp /
# sh) and assists, which gives PP / SH assists (the box score only has PP goals). No preseason
# PP ice time or PP share is published anywhere.

PRESEASON = 1
FINAL_STATES = ("FINAL", "OFF")
_BOX_SKATER_MAP = {"goals": "G", "assists": "A", "points": "PTS", "plusMinus": "PM", "pim": "PIM",
                   "hits": "HIT", "blockedShots": "BLK", "powerPlayGoals": "PPG", "sog": "SOG",
                   "shorthandedGoals": "SHG"}


class NhlSkaterGameLine(BaseModel):
    """One skater's box-score line in one game (built for preseason games, gameType 1).

    ``stats`` uses canonical keys (GP = 1). PPA / PPP / SHA / SHP are only present when the
    game's scoring summary was parsed (``has_strength``)."""
    player_id: int
    game_id: int
    game_type: int | None = None
    date: Date
    team: str | None = None
    opponent: str | None = None
    home: bool | None = None
    name: str | None = None
    position: str | None = None
    toi: float | None = None             # seconds
    shifts: int | None = None
    has_strength: bool = False
    stats: dict[str, float] = Field(default_factory=dict)


def _box_skater(row: dict, game: dict, side: str) -> NhlSkaterGameLine | None:
    if row.get("playerId") is None:
        return None
    home, away = game.get("homeTeam") or {}, game.get("awayTeam") or {}
    me, other = (home, away) if side == "homeTeam" else (away, home)
    stats = {"GP": 1.0}
    stats.update({canon: _num(row.get(src)) for src, canon in _BOX_SKATER_MAP.items() if src in row})
    shifts = row.get("shifts")
    return NhlSkaterGameLine(
        player_id=int(row["playerId"]), game_id=int(game.get("id") or 0), game_type=game.get("gameType"),
        date=_parse_date(game.get("gameDate")), team=me.get("abbrev"), opponent=other.get("abbrev"),
        home=side == "homeTeam", name=_default(row.get("name")), position=map_position(row.get("position")),
        toi=_secs(row.get("toi")), shifts=int(shifts) if isinstance(shifts, (int, float)) else None, stats=stats)


def boxscore_skater_lines(box: dict) -> list[NhlSkaterGameLine]:
    """Skater lines (forwards + defense, both teams) of a ``gamecenter/{id}/boxscore`` payload;
    [] when the game is not final or the box score carries no player stats (limited scoring)."""
    if (box.get("gameState") or "") not in FINAL_STATES:
        return []
    by_team = box.get("playerByGameStats") or {}
    out = []
    for side in ("awayTeam", "homeTeam"):
        team = by_team.get(side) or {}
        for group in ("forwards", "defense"):
            for row in team.get(group) or []:
                ln = _box_skater(row, box, side)
                if ln is not None:
                    out.append(ln)
    return out


def goal_strength_points(landing: dict) -> dict[int, dict[str, float]] | None:
    """player_id -> {PPG, PPA, SHG, SHA} from a ``gamecenter/{id}/landing`` scoring summary
    (goals with strength "pp" / "sh"); None when the payload has no scoring summary."""
    periods = (landing.get("summary") or {}).get("scoring")
    if periods is None:
        return None
    out: dict[int, dict[str, float]] = {}
    for per in periods:
        for g in per.get("goals") or []:
            strength = str(g.get("strength") or "").lower()
            if strength not in ("pp", "sh"):
                continue
            tag = strength.upper()
            if g.get("playerId") is not None:
                d = out.setdefault(int(g["playerId"]), {})
                d[f"{tag}G"] = d.get(f"{tag}G", 0.0) + 1.0
            for a in g.get("assists") or []:
                if a.get("playerId") is not None:
                    d = out.setdefault(int(a["playerId"]), {})
                    d[f"{tag}A"] = d.get(f"{tag}A", 0.0) + 1.0
    return out


def apply_goal_strength(lines: list[NhlSkaterGameLine], strength: dict[int, dict[str, float]] | None) -> None:
    """Add PPA / PPP / SHA / SHP from a scoring summary (goals keep the box score's PPG / SHG
    when it has them, else the summary's)."""
    if strength is None:
        return
    for ln in lines:
        s = strength.get(ln.player_id, {})
        for tag in ("PP", "SH"):
            g = ln.stats.get(f"{tag}G", s.get(f"{tag}G", 0.0))
            a = s.get(f"{tag}A", 0.0)
            ln.stats[f"{tag}G"] = g
            ln.stats[f"{tag}A"] = a
            ln.stats[f"{tag}P"] = g + a
        ln.has_strength = True


# --------------------------------------------------------------------------- client

class NhlClient:
    def __init__(self, fetch_json: FetchJson | None = None, season: int | None = None):
        self.fetch_json: FetchJson = fetch_json or default_fetch_json
        self.season = season or current_season()

    # ---- stats REST -----------------------------------------------------------
    def _report(self, kind: str, report: str, season: int | None, game_type: int,
                extra_filter: str | None = None) -> list[dict]:
        season = season or self.season
        cay = f"seasonId={season} and gameTypeId={game_type}"
        if extra_filter:
            cay += f" and {extra_filter}"
        params = {
            "isAggregate": "false",
            "isGame": "false",
            # sort must reference a field present in the report; playerId is in all of them
            "sort": json.dumps([{"property": "playerId", "direction": "ASC"}]),
            "start": 0,
            "limit": -1,
            "cayenneExp": cay,
        }
        url = f"{STATS_BASE}/{kind}/{report}"
        data = self.fetch_json(url, params) or {}
        rows = list(data.get("data") or [])
        total = data.get("total")
        # limit=-1 returns everything today; page defensively if the API ever caps it
        seen = {r.get("playerId") for r in rows}
        while isinstance(total, int) and 0 < len(rows) < total:
            page = self.fetch_json(url, {**params, "start": len(rows), "limit": 100}) or {}
            more = [r for r in page.get("data") or [] if r.get("playerId") not in seen]
            if not more:
                break
            seen.update(r.get("playerId") for r in more)
            rows.extend(more)
        return rows

    def skater_summary(self, season: int | None = None, game_type: int = 2) -> list[dict]:
        """Raw rows of the skater summary report."""
        return self._report("skater", "summary", season, game_type)

    def skater_realtime(self, season: int | None = None, game_type: int = 2) -> list[dict]:
        """Raw rows of the realtime report (hits, blockedShots, giveaways, takeaways, missedShots...)."""
        return self._report("skater", "realtime", season, game_type)

    def skater_faceoffs(self, season: int | None = None, game_type: int = 2) -> list[dict]:
        """Raw rows of the faceoffwins report (totalFaceoffWins, totalFaceoffLosses...)."""
        return self._report("skater", "faceoffwins", season, game_type)

    def all_skaters(self, season: int | None = None, game_type: int = 2,
                    include_realtime: bool = True, include_faceoffs: bool = True) -> list[NhlSkaterSeason]:
        """Skater season lines merged across summary + realtime (+ faceoffs) by playerId."""
        summary = self.skater_summary(season, game_type)
        realtime = {r["playerId"]: r for r in self.skater_realtime(season, game_type)} if include_realtime else {}
        faceoffs: dict[int, dict] = {}
        if include_faceoffs:
            try:
                faceoffs = {r["playerId"]: r for r in self.skater_faceoffs(season, game_type)}
            except Exception:  # optional report; FOW simply missing if it fails
                faceoffs = {}
        out = []
        for row in summary:
            pid = row["playerId"]
            out.append(_skater_from_rows(row, realtime.get(pid) if include_realtime else None,
                                         faceoffs.get(pid) if include_faceoffs else None, game_type))
            if include_realtime and pid not in realtime:
                for k in ("HIT", "BLK"):
                    out[-1].stats.setdefault(k, 0.0)
        return out

    def goalie_summary(self, season: int | None = None, game_type: int = 2) -> list[NhlGoalieSeason]:
        return [_goalie_from_row(r, game_type) for r in self._report("goalie", "summary", season, game_type)]

    all_goalies = goalie_summary

    # ---- web API: players ---------------------------------------------------------
    def player_landing(self, player_id: int) -> NhlPlayerLanding:
        d = self.fetch_json(f"{WEB_BASE}/player/{player_id}/landing", None) or {}
        featured = ((d.get("featuredStats") or {}).get("regularSeason") or {}).get("subSeason") or {}
        draft = d.get("draftDetails") or {}
        career = ((d.get("careerTotals") or {}).get("regularSeason") or {})

        def _int(v: Any) -> int | None:
            try:
                return int(v) if v not in (None, "") else None
            except (TypeError, ValueError):
                return None

        return NhlPlayerLanding(
            player_id=int(d.get("playerId") or player_id),
            first_name=_default(d.get("firstName")) or "",
            last_name=_default(d.get("lastName")) or "",
            team=d.get("currentTeamAbbrev"),
            position=map_position(d.get("position")),
            birth_date=_parse_date(d.get("birthDate")),
            shoots_catches=d.get("shootsCatches"),
            is_active=d.get("isActive"),
            last5=[_game_log_entry(g) for g in d.get("last5Games") or []],
            featured_regular_season=featured,
            season_totals=d.get("seasonTotals") or [],
            draft_year=_int(draft.get("year")),
            draft_round=_int(draft.get("round")),
            draft_overall=_int(draft.get("overallPick")),
            draft_team=draft.get("teamAbbrev"),
            career_gp=_int(career.get("gamesPlayed")) if career else (0 if d else None),
        )

    def player_history(self, player_id: int, landing: "NhlPlayerLanding | None" = None) -> list["LeagueSeason"]:
        """Regular-season lines in every league the player played in (``landing.seasonTotals``;
        WHL / OHL / SHL / Liiga / KHL / NCAA / AHL / NHL / tournaments...), oldest first, with his
        age on Oct 1 of each season. ``landing`` reuses an already parsed page (the enrich pedigree
        step fetched it); otherwise the page is fetched through ``fetch_json`` (the enrich fetchers
        cache ``/landing`` for 30 days, ``providers.enrich.TTL_LANDING``)."""
        landing = landing if landing is not None else self.player_landing(player_id)
        return parse_season_totals(landing.season_totals, landing.birth_date)

    def game_log(self, player_id: int, season: int | None = None, game_type: int = 2) -> list[NhlGameLogEntry]:
        """Per-game lines, most recent first (as returned by the API)."""
        season = season or self.season
        d = self.fetch_json(f"{WEB_BASE}/player/{player_id}/game-log/{season}/{game_type}", None) or {}
        entries = []
        for row in d.get("gameLog") or []:
            e = _game_log_entry(row)
            if e.game_type is None:
                e.game_type = d.get("gameTypeId", game_type)
            entries.append(e)
        return entries

    def search_player(self, q: str, active: bool | None = True, limit: int = 20) -> list[NhlSearchResult]:
        params: dict[str, Any] = {"culture": "en-us", "limit": limit, "q": q}
        if active is not None:
            params["active"] = "true" if active else "false"
        rows = self.fetch_json(SEARCH_URL, params) or []
        out = []
        for r in rows:
            try:
                pid = int(r["playerId"])
            except (KeyError, TypeError, ValueError):
                continue
            out.append(NhlSearchResult(
                player_id=pid,
                name=r.get("name") or "",
                position=map_position(r.get("positionCode")),
                position_code=r.get("positionCode"),
                team=r.get("teamAbbrev") or r.get("lastTeamAbbrev"),
                active=r.get("active"),
                sweater_number=r.get("sweaterNumber"),
                birth_country=r.get("birthCountry"),
            ))
        return out

    # ---- web API: schedules ---------------------------------------------------------
    def team_schedule(self, team: str, season: int | None = None, game_type: int | None = 2) -> list[NhlGame]:
        """Season schedule for one club; ``game_type=None`` keeps preseason/playoffs too."""
        season = season or self.season
        d = self.fetch_json(f"{WEB_BASE}/club-schedule-season/{team}/{season}", None) or {}
        games = [_game_from_row(g) for g in d.get("games") or []]
        if game_type is not None:
            games = [g for g in games if g.game_type == game_type]
        return sorted(games, key=lambda g: (g.date, g.game_id))

    def league_games(self, season: int | None = None, teams: Iterable[str] = NHL_TEAMS,
                     game_type: int | None = 2) -> dict[str, list[NhlGame]]:
        return {t: self.team_schedule(t, season, game_type) for t in teams}

    def league_schedule(self, season: int | None = None, teams: Iterable[str] = NHL_TEAMS,
                        game_type: int | None = 2) -> dict[str, list[date]]:
        """team -> sorted game dates for all 32 clubs (32 requests; cache upstream)."""
        return {t: [g.date for g in games] for t, games in self.league_games(season, teams, game_type).items()}

    def week_schedule(self, day: date | str | None = None) -> NhlWeekSchedule:
        """League schedule for the week starting at ``day`` (default: 'now')."""
        key = day.isoformat() if isinstance(day, date) else (day or "now")
        d = self.fetch_json(f"{WEB_BASE}/schedule/{key}", None) or {}
        days: dict[date, list[NhlGame]] = {}
        for gw in d.get("gameWeek") or []:
            dd = _parse_date(gw.get("date"))
            if dd is None:
                continue
            days[dd] = [_game_from_row(g, dd) for g in gw.get("games") or []]
        return NhlWeekSchedule(
            days=days,
            regular_season_start=_parse_date(d.get("regularSeasonStartDate")),
            regular_season_end=_parse_date(d.get("regularSeasonEndDate")),
            next_start_date=_parse_date(d.get("nextStartDate")),
            previous_start_date=_parse_date(d.get("previousStartDate")),
        )

    # ---- web API: rosters ---------------------------------------------------------
    def team_roster(self, team: str, season: int | None = None) -> list[NhlRosterPlayer]:
        season = season or self.season
        d = self.fetch_json(f"{WEB_BASE}/roster/{team}/{season}", None) or {}
        out = []
        for group in ("forwards", "defensemen", "goalies"):
            for p in d.get(group) or []:
                code = p.get("positionCode")
                out.append(NhlRosterPlayer(
                    player_id=int(p["id"]),
                    first_name=_default(p.get("firstName")) or "",
                    last_name=_default(p.get("lastName")) or "",
                    team=team,
                    position=map_position(code),
                    position_code=code,
                    birth_date=_parse_date(p.get("birthDate")),
                    sweater_number=p.get("sweaterNumber"),
                    shoots_catches=p.get("shootsCatches"),
                ))
        return out

    def league_rosters(self, season: int | None = None,
                       teams: Iterable[str] = NHL_TEAMS) -> dict[str, list[NhlRosterPlayer]]:
        return {t: self.team_roster(t, season) for t in teams}

    # ---- stats REST: per-game deployment (date windows) --------------------------------
    def _game_rows(self, kind: str, report: str, season: int | None, start: date | None, end: date | None,
                   game_type: int = 2) -> list[dict]:
        """Per-game rows (``isGame=true``) of a stats report for games in [start, end]."""
        season = season or self.season
        cay = f"seasonId={season} and gameTypeId={game_type}"
        extra = game_window_filter(start, end)
        if extra:
            cay += f" and {extra}"
        id_field = "teamId" if kind == "team" else "playerId"
        params = {
            "isAggregate": "false",
            "isGame": "true",
            # every sort property must be a field of the report: gameDate / gameId / the row id are
            "sort": json.dumps([{"property": "gameDate", "direction": "ASC"},
                                {"property": "gameId", "direction": "ASC"},
                                {"property": id_field, "direction": "ASC"}]),
            "start": 0,
            "limit": -1,
            "cayenneExp": cay,
        }
        url = f"{STATS_BASE}/{kind}/{report}"
        data = self.fetch_json(url, params) or {}
        rows = [r for r in data.get("data") or [] if r.get("gameDate") and r.get(id_field) is not None]
        total = data.get("total")
        seen = {(r.get("gameId"), r.get(id_field)) for r in rows}
        while isinstance(total, int) and 0 < len(rows) < total:
            page = self.fetch_json(url, {**params, "start": len(rows), "limit": 100}) or {}
            more = [r for r in page.get("data") or []
                    if r.get("gameDate") and r.get(id_field) is not None
                    and (r.get("gameId"), r.get(id_field)) not in seen]
            if not more:
                break
            seen.update((r.get("gameId"), r.get(id_field)) for r in more)
            rows.extend(more)
        return rows

    def skater_toi_games(self, season: int | None, start: date | None, end: date | None,
                         game_type: int = 2) -> list[NhlSkaterToiGame]:
        """Per-game skater ice time (all / EV / PP / SH seconds, shifts) for games in [start, end]."""
        return [_toi_game(r) for r in self._game_rows("skater", "timeonice", season, start, end, game_type)]

    def skater_pp_games(self, season: int | None, start: date | None, end: date | None,
                        game_type: int = 2) -> list[NhlSkaterPpGame]:
        """Per-game skater power play (PP seconds, share of the team's PP time, PP goals / points)."""
        return [_pp_game(r) for r in self._game_rows("skater", "powerplay", season, start, end, game_type)]

    def goalie_games(self, season: int | None, start: date | None, end: date | None,
                     game_type: int = 2) -> list[NhlGoalieGame]:
        """Per-game goalie lines (started, SA, SV, GA, TOI) for games in [start, end]."""
        return [_goalie_game(r) for r in self._game_rows("goalie", "summary", season, start, end, game_type)]

    def team_pp_toi_games(self, season: int | None, start: date | None, end: date | None,
                          game_type: int = 2) -> list[NhlTeamPpGame]:
        """Per-game team power-play time (seconds) and opportunities for games in [start, end]."""
        return _team_pp_games(self._game_rows("team", "powerplay", season, start, end, game_type))

    def skater_deployment_season(self, season: int | None = None,
                                 game_type: int = 2) -> dict[int, NhlSkaterDeploymentSeason]:
        """player_id -> season TOI / PP deployment from the season (``isGame=false``) timeonice
        and powerplay reports; empty before the season's first game."""
        season = season or self.season
        toi = self._report("skater", "timeonice", season, game_type)
        try:
            pp = {r.get("playerId"): r for r in self._report("skater", "powerplay", season, game_type)}
        except Exception:  # PP share is optional
            pp = {}
        out: dict[int, NhlSkaterDeploymentSeason] = {}
        for r in toi:
            if r.get("playerId") is None:
                continue
            pid = int(r["playerId"])
            p = pp.get(r["playerId"]) or {}
            teams = split_teams(r.get("teamAbbrevs"))
            pct = p.get("ppTimeOnIcePctPerGame")
            out[pid] = NhlSkaterDeploymentSeason(
                player_id=pid, season=int(r.get("seasonId") or season), name=r.get("skaterFullName"),
                team=teams[-1] if teams else None, teams=teams, gp=int(_num(r.get("gamesPlayed"))),
                toi_per_game=_secs(r.get("timeOnIcePerGame")), ev_toi_per_game=_secs(r.get("evTimeOnIcePerGame")),
                pp_toi_per_game=_secs(r.get("ppTimeOnIcePerGame")),
                sh_toi_per_game=_secs(r.get("shTimeOnIcePerGame")),
                pp_share=float(pct) if isinstance(pct, (int, float)) and not isinstance(pct, bool) else None)
        return out

    # ---- preseason (gameType 1; see the "preseason" section above) ---------------------
    def preseason_game_logs(self, player_id: int, season: int | None = None) -> list[NhlGameLogEntry]:
        """The player's preseason game log (``/game-log/{season}/1``). As of 2026-09 the NHL
        returns no ``gameLog`` for preseason, so this is [] in practice; the box scores
        (:meth:`preseason_skater_games`) are the source that works."""
        return self.game_log(player_id, season, game_type=PRESEASON)

    def preseason_games(self, season: int | None = None, teams: Iterable[str] = NHL_TEAMS,
                        before: date | None = None) -> list[NhlGame]:
        """Unique preseason games (gameType 1) of ``teams`` dated before ``before`` (all when
        None), from the club schedules (the same payloads as the regular-season schedule)."""
        seen: dict[int, NhlGame] = {}
        for t in teams:
            for g in self.team_schedule(t, season, game_type=PRESEASON):
                if before is None or g.date < before:
                    seen.setdefault(g.game_id, g)
        return sorted(seen.values(), key=lambda g: (g.date, g.game_id))

    def game_skater_lines(self, game_id: int, strength: bool = True) -> list[NhlSkaterGameLine]:
        """Skater box-score lines of one finished game; with ``strength`` the game's scoring
        summary adds PP / SH assists and points (a failing summary only drops those)."""
        lines = boxscore_skater_lines(self.fetch_json(f"{WEB_BASE}/gamecenter/{game_id}/boxscore", None) or {})
        if lines and strength:
            try:
                landing = self.fetch_json(f"{WEB_BASE}/gamecenter/{game_id}/landing", None) or {}
            except Exception:
                landing = {}
            apply_goal_strength(lines, goal_strength_points(landing))
        return lines

    def preseason_skater_games(self, season: int | None = None, teams: Iterable[str] = NHL_TEAMS,
                               before: date | None = None) -> list[NhlSkaterGameLine]:
        """Per-game skater lines (G / A / PTS / SOG / PPP / TOI ...) of every finished preseason
        game: 1 box score + 1 scoring summary per game (the stats REST reports carry no
        preseason; see above). Games whose box score fails are skipped."""
        out: list[NhlSkaterGameLine] = []
        for g in self.preseason_games(season, teams, before):
            try:
                out.extend(self.game_skater_lines(g.game_id))
            except Exception:
                continue
        return out


# --------------------------------------------------------------------------- league history / NHLe
#
# ``player/{id}/landing`` -> ``seasonTotals``: one row per (season, league, team, game type), e.g.
# (verified live 2026-09-29 on Ivar Stenberg 8486103, Anton Frondell 8485391, Jimmy Snuggerud
# 8483516, Ivan Demidov 8484984 and a dozen other prospects) ``{"season": 20252026, "gameTypeId":
# 2, "leagueAbbrev": "SHL", "teamName": {"default": "Frölunda HC"}, "gamesPlayed": 43, "goals": 11,
# "assists": 22, "points": 33, "sequence": ...}``. gameTypeId 2 = regular season, 3 = playoffs.
# Goalies' rows carry no points. League strings seen: NHL, AHL, KHL, SHL, Liiga, Czechia, NLA, DEL,
# NCAA, OHL, WHL, QMJHL, USHL, NTDP, MHL, VHL, HockeyAllsvenskan, J20 Nationell, WJC-20, WC, ...

NHLE_FACTORS_PATH = Path(__file__).resolve().parent.parent / "valuation" / "nhle_factors.json"


class LeagueSeason(BaseModel):
    """One regular-season line in any league (``landing.seasonTotals``)."""
    season: int                       # e.g. 20252026
    league: str                       # leagueAbbrev: "SHL", "OHL", "AHL", "NHL", "WJC-20", ...
    team: str | None = None
    gp: int
    g: int = 0
    a: int = 0
    pts: int = 0
    age_at_season: float | None = None  # age on Oct 1 of the season's start year

    @property
    def start_year(self) -> int:
        return self.season // 10000

    @property
    def pts_per_game(self) -> float:
        return self.pts / self.gp if self.gp else 0.0


def _age_oct1(birth: date | None, start_year: int) -> float | None:
    if birth is None:
        return None
    return round((date(start_year, 10, 1) - birth).days / 365.25, 2)


def parse_season_totals(rows: Iterable[dict] | None, birth_date: date | None = None,
                        game_type: int = 2) -> list[LeagueSeason]:
    """``seasonTotals`` rows of ``game_type`` (2 = regular season) with games played, oldest first.
    Points default to goals + assists when missing (goalies carry neither and get 0)."""
    out: list[LeagueSeason] = []
    for r in rows or []:
        try:
            if int(r.get("gameTypeId") or 0) != game_type:
                continue
            gp = int(r.get("gamesPlayed") or 0)
            season = int(r.get("season"))
        except (TypeError, ValueError):
            continue
        if gp <= 0:
            continue
        g, a = int(r.get("goals") or 0), int(r.get("assists") or 0)
        pts = r.get("points")
        out.append(LeagueSeason(season=season, league=str(r.get("leagueAbbrev") or "?"),
                                team=_default(r.get("teamName")), gp=gp, g=g, a=a,
                                pts=int(pts) if pts is not None else g + a,
                                age_at_season=_age_oct1(birth_date, season // 10000)))
    out.sort(key=lambda x: (x.season, int(x.league != "NHL")))
    return out


_NHLE_CACHE: dict[str, Any] = {}


def load_nhle_factors(path: str | None = None) -> dict[str, Any]:
    """The NHLe table (``valuation/nhle_factors.json``): {"leagues": {abbrev: {factor, class,
    reliability}}, "norm_age": {...}, "age_adjustment": {...}, "version": ...}. Approximate
    practitioner midpoints, kept as data so they can be refit later. Cached per path."""
    key = str(path or NHLE_FACTORS_PATH)
    if key not in _NHLE_CACHE:
        with open(key, encoding="utf-8") as f:
            _NHLE_CACHE[key] = json.load(f)
    return _NHLE_CACHE[key]


# The packaged default table (a small local file, read once at import).
NHLE_FACTORS: dict[str, Any] = load_nhle_factors()
