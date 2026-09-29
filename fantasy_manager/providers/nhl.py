"""NHL data provider: season stats (stats REST API) and schedules, rosters, game logs,
player pages and search (api-web.nhle.com).

All network access goes through an injected ``fetch_json(url, params)`` callable so the
integrator can swap in a caching fetcher and tests can replay recorded fixtures.
"""

from __future__ import annotations

import json
from collections import Counter
from datetime import date, datetime, timedelta
from datetime import date as Date
from typing import Any, Callable, Iterable, Literal

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
