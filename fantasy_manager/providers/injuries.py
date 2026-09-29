"""NHL injury report from ESPN's public site API."""

from __future__ import annotations

import re
from datetime import date, datetime
from typing import Any, Callable, Literal

import httpx
from pydantic import BaseModel

FetchJson = Callable[[str, "dict | None"], Any]

INJURIES_URL = "https://site.api.espn.com/apis/site/v2/sports/hockey/nhl/injuries"

InjuryStatus = Literal["dtd", "out", "ir", "ltir", "suspended", "unknown"]

# ESPN team display name -> NHL abbreviation.
ESPN_TEAM_TO_NHL: dict[str, str] = {
    "Anaheim Ducks": "ANA",
    "Boston Bruins": "BOS",
    "Buffalo Sabres": "BUF",
    "Calgary Flames": "CGY",
    "Carolina Hurricanes": "CAR",
    "Chicago Blackhawks": "CHI",
    "Colorado Avalanche": "COL",
    "Columbus Blue Jackets": "CBJ",
    "Dallas Stars": "DAL",
    "Detroit Red Wings": "DET",
    "Edmonton Oilers": "EDM",
    "Florida Panthers": "FLA",
    "Los Angeles Kings": "LAK",
    "Minnesota Wild": "MIN",
    "Montreal Canadiens": "MTL",
    "Montréal Canadiens": "MTL",
    "Nashville Predators": "NSH",
    "New Jersey Devils": "NJD",
    "New York Islanders": "NYI",
    "New York Rangers": "NYR",
    "Ottawa Senators": "OTT",
    "Philadelphia Flyers": "PHI",
    "Pittsburgh Penguins": "PIT",
    "San Jose Sharks": "SJS",
    "Seattle Kraken": "SEA",
    "St. Louis Blues": "STL",
    "Tampa Bay Lightning": "TBL",
    "Toronto Maple Leafs": "TOR",
    "Utah Mammoth": "UTA",
    "Utah Hockey Club": "UTA",
    "Utah HC": "UTA",
    "Vancouver Canucks": "VAN",
    "Vegas Golden Knights": "VGK",
    "Washington Capitals": "WSH",
    "Winnipeg Jets": "WPG",
}

# ESPN team abbreviation -> NHL abbreviation (only where they differ).
ESPN_ABBREV_TO_NHL: dict[str, str] = {
    "NJ": "NJD", "SJ": "SJS", "TB": "TBL", "LA": "LAK", "UTAH": "UTA", "WAS": "WSH",
}

_ESPN_ID_RE = re.compile(r"/id/(\d+)")


class InjuryReport(BaseModel):
    player_name: str
    espn_player_id: int | None = None
    team_name: str | None = None      # ESPN team group the entry was listed under
    team: str | None = None           # NHL abbreviation (athlete's own team if given)
    position: str | None = None       # ESPN abbreviation, e.g. LW / D / G
    status_raw: str
    status: InjuryStatus
    injury_type: str | None = None    # e.g. "Upper Body", "Hip", "Suspension"
    return_date: date | None = None   # ESPN's estimated return
    comment: str | None = None
    updated: datetime | None = None

    @property
    def is_out(self) -> bool:
        return self.status in ("out", "ir", "ltir", "suspended")


def default_fetch_json(url: str, params: dict | None = None) -> Any:
    resp = httpx.get(url, params=params, follow_redirects=True, timeout=20.0,
                     headers={"User-Agent": "fantasy-manager/0.1"})
    resp.raise_for_status()
    return resp.json()


def map_status(raw: str | None, type_name: str | None = None) -> InjuryStatus:
    """Map ESPN's status text (or its INJURY_STATUS_* type name) to a canonical status."""
    text = " ".join(x for x in (raw, type_name) if x).lower().replace("_", " ")
    if not text:
        return "unknown"
    if "long-term" in text or "long term" in text or "ltir" in text:
        return "ltir"
    if "suspen" in text:
        return "suspended"
    if "injured reserve" in text or re.search(r"\bir\b", text):
        return "ir"
    if "day-to-day" in text or "daytoday" in text or "day to day" in text or re.search(r"\bdtd\b", text) \
            or "questionable" in text or "probable" in text:
        return "dtd"
    if re.search(r"\bout\b", text) or "doubtful" in text or "week-to-week" in text:
        return "out"
    return "unknown"


def team_to_nhl(display_name: str | None = None, espn_abbrev: str | None = None) -> str | None:
    if espn_abbrev:
        a = espn_abbrev.strip().upper()
        return ESPN_ABBREV_TO_NHL.get(a, a)
    if display_name:
        return ESPN_TEAM_TO_NHL.get(display_name.strip())
    return None


def _parse_espn_dt(value: str | None) -> datetime | None:
    if not value:
        return None
    v = value.replace("Z", "+00:00")
    # ESPN often omits seconds: 2026-09-27T13:59+00:00 (fromisoformat handles it in 3.11+)
    try:
        return datetime.fromisoformat(v)
    except ValueError:
        return None


def _parse_espn_id(athlete: dict) -> int | None:
    for link in athlete.get("links") or []:
        m = _ESPN_ID_RE.search(link.get("href") or "")
        if m:
            return int(m.group(1))
    uid = athlete.get("uid") or ""
    m = re.search(r"a:(\d+)", uid)
    return int(m.group(1)) if m else None


def parse_injuries(payload: dict) -> list[InjuryReport]:
    out: list[InjuryReport] = []
    for team in payload.get("injuries") or []:
        team_name = team.get("displayName")
        for inj in team.get("injuries") or []:
            ath = inj.get("athlete") or {}
            name = ath.get("displayName") or " ".join(
                x for x in (ath.get("firstName"), ath.get("lastName")) if x)
            if not name:
                continue
            details = inj.get("details") or {}
            type_name = (inj.get("type") or {}).get("name")
            status_raw = inj.get("status") or (inj.get("type") or {}).get("description") or ""
            ath_team = ath.get("team") or {}
            comment = inj.get("longComment") or inj.get("shortComment")
            ret = details.get("returnDate")
            out.append(InjuryReport(
                player_name=name,
                espn_player_id=_parse_espn_id(ath),
                team_name=team_name,
                team=team_to_nhl(ath_team.get("displayName"), ath_team.get("abbreviation"))
                or team_to_nhl(team_name),
                position=(ath.get("position") or {}).get("abbreviation"),
                status_raw=status_raw,
                status=map_status(status_raw, type_name),
                injury_type=details.get("type"),
                return_date=date.fromisoformat(ret[:10]) if ret else None,
                comment=comment,
                updated=_parse_espn_dt(inj.get("date")),
            ))
    return out


def fetch_injuries(fetch_json: FetchJson | None = None) -> list[InjuryReport]:
    fetch_json = fetch_json or default_fetch_json
    return parse_injuries(fetch_json(INJURIES_URL, None) or {})
