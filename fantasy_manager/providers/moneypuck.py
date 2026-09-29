"""MoneyPuck expected-goals data (season summaries per skater / goalie).

MoneyPuck publishes free CSV season summaries (https://moneypuck.com/data.htm), free for
non-commercial use with credit (show ``CREDIT`` wherever the numbers appear). Only the published
files are downloaded; no other page is fetched:

    https://moneypuck.com/moneypuck/playerData/seasonSummary/{YEAR}/regular/skaters.csv
    https://moneypuck.com/moneypuck/playerData/seasonSummary/{YEAR}/regular/goalies.csv

``YEAR`` is the season's start year (2025 = 2025-26). The current season's file is refreshed
nightly (before opening night it is a header-only file); finished seasons never change. Each
player has one row per ``situation``: ``all``, ``5on5``, ``5on4`` (power play), ``4on5``
(penalty kill) and ``other``. ``playerId`` is the NHL player id. ``icetime`` is in seconds and
percentages (``onIce_xGoalsPercentage``) are fractions.

All network access goes through an injected ``fetch_text(url, params)`` callable (like
``NhlClient.fetch_json``); ``cached_fetch_text(cache)`` routes it through ``HttpCache`` with
12 h (current season) / 30 d (finished seasons) TTLs.
"""
from __future__ import annotations

import csv
import io
from dataclasses import dataclass
from datetime import date
from typing import Any, Callable, Iterable, Literal

import httpx

from .nhl import map_position

FetchText = Callable[[str, "dict | None"], str]

CREDIT = "Expected goals: MoneyPuck.com"
DATA_URL = "https://moneypuck.com/data.htm"
BASE_URL = "https://moneypuck.com/moneypuck/playerData/seasonSummary"

HOUR = 3600.0
TTL_CURRENT = 12 * HOUR
TTL_PAST = 30 * 24 * HOUR

Situation = Literal["all", "5on5", "5on4", "4on5", "other"]
SITUATIONS: tuple[str, ...] = ("all", "5on5", "5on4", "4on5", "other")
PP_SITUATION = "5on4"
# Older MoneyPuck files used dotted abbreviations for some clubs.
_TEAM_FIX = {"L.A": "LAK", "N.J": "NJD", "S.J": "SJS", "T.B": "TBL"}


# --------------------------------------------------------------------------- helpers

def season_url(season_start_year: int, kind: Literal["skaters", "goalies"] = "skaters") -> str:
    return f"{BASE_URL}/{int(season_start_year)}/regular/{kind}.csv"


def start_year(season: int) -> int:
    """20252026 -> 2025; 2025 -> 2025."""
    season = int(season)
    return season // 10000 if season > 9999 else season


def current_start_year(today: date | None = None) -> int:
    """Start year of the current NHL season (from September on, the upcoming one)."""
    today = today or date.today()
    return today.year if today.month >= 9 else today.year - 1


def ttl_for_url(url: str, today: date | None = None) -> float:
    """12 h for the current (or a future) season's file, 30 d for finished seasons."""
    cur = current_start_year(today)
    for part in url.split("/"):
        if len(part) == 4 and part.isdigit():
            return TTL_CURRENT if int(part) >= cur else TTL_PAST
    return TTL_CURRENT


def default_fetch_text(url: str, params: dict | None = None) -> str:
    """Plain uncached GET returning the response text."""
    resp = httpx.get(url, params=params, follow_redirects=True, timeout=60.0,
                     headers={"User-Agent": "fantasy-manager/0.1"})
    resp.raise_for_status()
    return resp.text


def cached_fetch_text(cache: Any, today: date | None = None) -> FetchText:
    """fetch_text through ``HttpCache.get_text`` with the season-dependent TTL."""
    def fetch(url: str, params: dict | None = None) -> str:
        return cache.get_text(url, params=params, ttl=ttl_for_url(url, today))
    return fetch


def _f(row: dict[str, str], key: str) -> float:
    v = row.get(key)
    if v in (None, ""):
        return 0.0
    try:
        return float(v)
    except ValueError:
        return 0.0


def _ratio(num: float, den: float) -> float | None:
    return num / den if den > 0 else None


def _team(code: str | None) -> str | None:
    code = (code or "").strip()
    return _TEAM_FIX.get(code, code) or None


# --------------------------------------------------------------------------- rows

@dataclass
class MpSkater:
    """One skater-season-situation row in canonical units (totals; TOI in minutes).

    ``onice_sh_pct`` = OnIce_F_goals / OnIce_F_shotsOnGoal (team shooting % with him on the ice),
    ``onice_sv_pct`` = 1 - OnIce_A_goals / OnIce_A_shotsOnGoal. ``pp_*`` come from the 5on4 rows
    and are only filled when requested (``skaters(..., with_pp=True)``)."""
    nhl_id: int
    season: int                 # start year (2025 = 2025-26)
    name: str
    team: str | None
    position: str | None        # C / LW / RW / D
    situation: str
    gp: int
    toi_min: float
    ixg: float                  # I_F_xGoals
    goals: float                # I_F_goals
    sog: float                  # I_F_shotsOnGoal
    ixg_adj: float              # I_F_scoreVenueAdjustedxGoals
    shot_attempts: float = 0.0
    points: float = 0.0
    primary_assists: float = 0.0
    secondary_assists: float = 0.0
    onice_xg_pct: float | None = None       # onIce_xGoalsPercentage (0..1)
    onice_corsi_pct: float | None = None
    onice_gf: float = 0.0                   # OnIce_F_goals
    onice_sog_for: float = 0.0              # OnIce_F_shotsOnGoal
    onice_xgf: float = 0.0                  # OnIce_F_xGoals
    onice_ga: float = 0.0
    onice_sog_against: float = 0.0
    onice_sh_pct: float | None = None
    onice_sv_pct: float | None = None
    pp_toi_min: float | None = None
    pp_ixg: float | None = None
    pp_goals: float | None = None
    pp_points: float | None = None

    @property
    def group(self) -> str:
        return "D" if self.position == "D" else "F"

    @property
    def assists(self) -> float:
        return self.primary_assists + self.secondary_assists

    @property
    def goals_minus_ixg(self) -> float:
        return self.goals - self.ixg

    @property
    def ixg_per_game(self) -> float | None:
        return _ratio(self.ixg, self.gp)

    @property
    def sh_pct(self) -> float | None:
        return _ratio(self.goals, self.sog)

    @property
    def ixg_per_shot(self) -> float | None:
        """ixG per shot on goal: the shooting % his shot quality implies."""
        return _ratio(self.ixg, self.sog)


@dataclass
class MpGoalie:
    nhl_id: int
    season: int
    name: str
    team: str | None
    situation: str
    gp: int
    toi_min: float
    xga: float                  # xGoals against
    ga: float                   # goals against
    sa: float                   # shots on goal against (``ongoal``)
    unblocked_attempts: float = 0.0

    @property
    def sv_pct(self) -> float | None:
        return None if self.sa <= 0 else 1.0 - self.ga / self.sa

    @property
    def xsv_pct(self) -> float | None:
        """Save % an average goalie would post on the same shots."""
        return None if self.sa <= 0 else 1.0 - self.xga / self.sa

    @property
    def gsax(self) -> float:
        """Goals saved above expected (positive = better than expected)."""
        return self.xga - self.ga


def parse_skater_row(row: dict[str, str]) -> MpSkater:
    ogf, osf = _f(row, "OnIce_F_goals"), _f(row, "OnIce_F_shotsOnGoal")
    oga, osa = _f(row, "OnIce_A_goals"), _f(row, "OnIce_A_shotsOnGoal")
    xgp = row.get("onIce_xGoalsPercentage")
    cfp = row.get("onIce_corsiPercentage")
    return MpSkater(
        nhl_id=int(row["playerId"]), season=int(_f(row, "season")), name=(row.get("name") or "").strip(),
        team=_team(row.get("team")), position=map_position(row.get("position")),
        situation=(row.get("situation") or "").strip(), gp=int(_f(row, "games_played")),
        toi_min=_f(row, "icetime") / 60.0, ixg=_f(row, "I_F_xGoals"), goals=_f(row, "I_F_goals"),
        sog=_f(row, "I_F_shotsOnGoal"), ixg_adj=_f(row, "I_F_scoreVenueAdjustedxGoals"),
        shot_attempts=_f(row, "I_F_shotAttempts"), points=_f(row, "I_F_points"),
        primary_assists=_f(row, "I_F_primaryAssists"), secondary_assists=_f(row, "I_F_secondaryAssists"),
        onice_xg_pct=float(xgp) if xgp not in (None, "") else None,
        onice_corsi_pct=float(cfp) if cfp not in (None, "") else None,
        onice_gf=ogf, onice_sog_for=osf, onice_xgf=_f(row, "OnIce_F_xGoals"), onice_ga=oga,
        onice_sog_against=osa, onice_sh_pct=_ratio(ogf, osf),
        onice_sv_pct=None if osa <= 0 else 1.0 - oga / osa)


def parse_goalie_row(row: dict[str, str]) -> MpGoalie:
    return MpGoalie(
        nhl_id=int(row["playerId"]), season=int(_f(row, "season")), name=(row.get("name") or "").strip(),
        team=_team(row.get("team")), situation=(row.get("situation") or "").strip(),
        gp=int(_f(row, "games_played")), toi_min=_f(row, "icetime") / 60.0, xga=_f(row, "xGoals"),
        ga=_f(row, "goals"), sa=_f(row, "ongoal"), unblocked_attempts=_f(row, "unblocked_shot_attempts"))


def read_csv(text: str) -> list[dict[str, str]]:
    """Rows of a MoneyPuck CSV (a header-only file -> [])."""
    text = text.lstrip("﻿")
    return [r for r in csv.DictReader(io.StringIO(text)) if r.get("playerId")]


def parse_skaters(text: str, situation: str | None = "all", with_pp: bool = False) -> list[MpSkater]:
    """Skater rows of one situation (``None`` = every situation). ``with_pp`` fills the ``pp_*``
    fields of each row from the same player's 5on4 row."""
    rows = [parse_skater_row(r) for r in read_csv(text)]
    out = [r for r in rows if situation is None or r.situation == situation]
    if with_pp:
        pp = {r.nhl_id: r for r in rows if r.situation == PP_SITUATION}
        for r in out:
            p = pp.get(r.nhl_id)
            if p is not None:
                r.pp_toi_min, r.pp_ixg, r.pp_goals, r.pp_points = p.toi_min, p.ixg, p.goals, p.points
            else:
                r.pp_toi_min = r.pp_ixg = r.pp_goals = r.pp_points = 0.0
    return out


def parse_goalies(text: str, situation: str | None = "all") -> list[MpGoalie]:
    return [g for g in (parse_goalie_row(r) for r in read_csv(text))
            if situation is None or g.situation == situation]


def onice_sh_norms(rows: Iterable[MpSkater], min_toi_min: float = 200.0) -> dict[str, float]:
    """Pooled on-ice shooting % by position group (F / D): sum(OnIce_F_goals) /
    sum(OnIce_F_shotsOnGoal) over skaters with >= ``min_toi_min`` minutes. Pass rows of one
    situation (normally ``all``)."""
    g: dict[str, float] = {}
    s: dict[str, float] = {}
    for r in rows:
        if r.toi_min < min_toi_min:
            continue
        g[r.group] = g.get(r.group, 0.0) + r.onice_gf
        s[r.group] = s.get(r.group, 0.0) + r.onice_sog_for
    return {grp: g[grp] / s[grp] for grp in g if s.get(grp, 0.0) > 0}


# --------------------------------------------------------------------------- client

class MoneyPuckClient:
    """Season summaries from MoneyPuck. Raw CSV text is memoised per (kind, year) so asking
    for several situations of one season downloads (or reads the cache) once."""

    def __init__(self, fetch_text: FetchText | None = None):
        self.fetch_text = fetch_text or default_fetch_text
        self._text: dict[tuple[str, int], str] = {}

    @classmethod
    def from_cache(cls, cache: Any, today: date | None = None) -> "MoneyPuckClient":
        return cls(cached_fetch_text(cache, today))

    def raw(self, season_start_year: int, kind: Literal["skaters", "goalies"] = "skaters") -> str:
        key = (kind, start_year(season_start_year))
        if key not in self._text:
            self._text[key] = self.fetch_text(season_url(key[1], kind), None)
        return self._text[key]

    def skaters(self, season_start_year: int, situation: str | None = "all",
                with_pp: bool = False) -> list[MpSkater]:
        """Skater rows of season ``season_start_year`` (2025 = 2025-26; NHL ids like 20252026
        are accepted too) for one ``situation`` (``None`` = all situations)."""
        if situation is not None and situation not in SITUATIONS:
            raise ValueError(f"unknown situation {situation!r}; choose from {', '.join(SITUATIONS)}")
        return parse_skaters(self.raw(season_start_year, "skaters"), situation, with_pp)

    def goalies(self, season_start_year: int, situation: str | None = "all") -> list[MpGoalie]:
        if situation is not None and situation not in SITUATIONS:
            raise ValueError(f"unknown situation {situation!r}; choose from {', '.join(SITUATIONS)}")
        return parse_goalies(self.raw(season_start_year, "goalies"), situation)

    def skaters_by_id(self, season_start_year: int, situation: str = "all",
                      with_pp: bool = False) -> dict[int, MpSkater]:
        return {r.nhl_id: r for r in self.skaters(season_start_year, situation, with_pp)}


def xg_calibration(rows: Iterable[MpSkater]) -> dict[str, float]:
    """League goals / ixG by position group (F / D) over one season's rows (situation ``all``).
    MoneyPuck's model runs a few % off by season and underrates defensemen by ~5%; multiply an
    ixG rate by this factor to put it on the scale of actual goals."""
    g: dict[str, float] = {}
    x: dict[str, float] = {}
    for r in rows:
        g[r.group] = g.get(r.group, 0.0) + r.goals
        x[r.group] = x.get(r.group, 0.0) + r.ixg
    return {grp: g[grp] / x[grp] for grp in g if x.get(grp, 0.0) > 0}
