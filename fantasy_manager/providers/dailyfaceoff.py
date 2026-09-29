"""Daily Faceoff line combinations, power-play / penalty-kill units and starting goalies.

Both page types are Next.js pages that embed their data in a ``<script id="__NEXT_DATA__">``
JSON blob. The parsers walk that JSON for the known keys instead of hard-coding the full
path, so a reshuffled page tree keeps working as long as the record shapes survive:

* team line pages (``/teams/{slug}/line-combinations``): a dict with a ``players`` list whose
  entries carry ``groupIdentifier`` (f1..f4, d1..d3, g, pp1, pp2, pk1, pk2, ir), ``name``,
  ``playerId`` (Daily Faceoff id), ``positionIdentifier`` (lw / c / rw / ld / rd / g1 / g2 on
  even-strength rows), ``injuryStatus``, ``gameTimeDecision`` and ``last5`` / ``last10``
  (games, total TOI minutes + seconds, PPP). The same dict holds ``teamAbbreviation``,
  ``updatedAt`` and the citation (``sourceName`` / ``source``).
* starting goalies (``/starting-goalies/{YYYY-MM-DD}``): game dicts with ``home*`` / ``away*``
  keys: ``TeamSlug``, ``GoalieId``, ``GoalieName``, ``NewsStrengthName`` (Confirmed, Likely,
  Expected, Unconfirmed or null), ``NewsSourceName``, ``NewsCreatedAt``; plus ``dateGmt``.

Good-citizen rules (robots.txt only disallows /api/ and /cms/; the terms page was not
reachable): an honest User-Agent, one fetch per team page per 12 hours and the goalie page at
most every 3 hours through :func:`make_cached_fetch_text` (``HttpCache.get_text``).
"""
from __future__ import annotations

import json
import logging
import re
from datetime import date, datetime
from typing import Any, Callable, Iterable, Iterator

import httpx
from pydantic import BaseModel, Field

from ..matching.normalize import normalize_team

log = logging.getLogger(__name__)

FetchText = Callable[[str, "dict | None"], str]

BASE_URL = "https://www.dailyfaceoff.com"
USER_AGENT = "fantasy-manager/0.1 (personal use)"
HOUR = 3600.0
TTL_LINES = 12 * HOUR
TTL_GOALIES = 3 * HOUR

# NHL abbreviation -> Daily Faceoff team slug (verified against the /teams index, 2026-09-29).
TEAM_SLUGS: dict[str, str] = {
    "ANA": "anaheim-ducks",
    "BOS": "boston-bruins",
    "BUF": "buffalo-sabres",
    "CGY": "calgary-flames",
    "CAR": "carolina-hurricanes",
    "CHI": "chicago-blackhawks",
    "COL": "colorado-avalanche",
    "CBJ": "columbus-blue-jackets",
    "DAL": "dallas-stars",
    "DET": "detroit-red-wings",
    "EDM": "edmonton-oilers",
    "FLA": "florida-panthers",
    "LAK": "los-angeles-kings",
    "MIN": "minnesota-wild",
    "MTL": "montreal-canadiens",
    "NSH": "nashville-predators",
    "NJD": "new-jersey-devils",
    "NYI": "new-york-islanders",
    "NYR": "new-york-rangers",
    "OTT": "ottawa-senators",
    "PHI": "philadelphia-flyers",
    "PIT": "pittsburgh-penguins",
    "SJS": "san-jose-sharks",
    "SEA": "seattle-kraken",
    "STL": "st-louis-blues",
    "TBL": "tampa-bay-lightning",
    "TOR": "toronto-maple-leafs",
    "UTA": "utah-mammoth",
    "VAN": "vancouver-canucks",
    "VGK": "vegas-golden-knights",
    "WSH": "washington-capitals",
    "WPG": "winnipeg-jets",
}
SLUG_TO_TEAM: dict[str, str] = {v: k for k, v in TEAM_SLUGS.items()}

EV_GROUPS = ("f1", "f2", "f3", "f4", "d1", "d2", "d3", "d4", "g")
PP_GROUPS = ("pp1", "pp2")
PK_GROUPS = ("pk1", "pk2")
# Goalie-page news strengths that name the starter (Unconfirmed / null do not).
START_STRENGTHS = ("confirmed", "likely", "expected")

_NEXT_DATA_RE = re.compile(r'<script[^>]*\bid=["\']__NEXT_DATA__["\'][^>]*>(.*?)</script>', re.S | re.I)
_POS = {"lw": "LW", "c": "C", "rw": "RW", "ld": "D", "rd": "D", "d": "D"}


class DfoError(Exception):
    """A Daily Faceoff page could not be fetched or parsed."""


class LinePlayer(BaseModel):
    """One player on a team's line-combinations page (all his groups merged)."""
    name: str
    dfo_id: int | None = None
    fantasydata_id: int | None = None
    position: str | None = None       # C / LW / RW / D / G (from the even-strength row)
    group: str                        # primary group: the even-strength line (f1.., d1.., g) or "ir"
    groups: list[str] = Field(default_factory=list)   # every group he is listed in
    line: str | None = None           # f1..f4, d1..d4, g (None: not in the even-strength lineup)
    goalie_depth: int | None = None   # 1 / 2 from g1 / g2
    pp_unit: str | None = None        # pp1 / pp2
    pk_unit: str | None = None        # pk1 / pk2
    injury_status: str | None = None  # Daily Faceoff's own tag, e.g. "out", "dtd", "ir"
    gtd: bool = False                 # game-time decision
    toi_last5: float | None = None    # minutes per game over the last 5 / 10 games
    toi_last10: float | None = None
    ppp_last5: int | None = None
    ppp_last10: int | None = None
    gp_last5: int | None = None
    gp_last10: int | None = None

    @property
    def in_lineup(self) -> bool:
        return self.line is not None


class TeamLines(BaseModel):
    team: str                          # NHL abbreviation
    slug: str | None = None
    updated_at: datetime | None = None
    source: str | None = None          # who reported the lines (sourceName), e.g. a beat writer
    source_url: str | None = None
    players: list[LinePlayer] = Field(default_factory=list)

    def by_group(self, group: str) -> list[LinePlayer]:
        return [p for p in self.players if group in p.groups]


class GoalieStart(BaseModel):
    game: str                          # "AWAY@HOME", e.g. "FLA@CAR"
    game_time: datetime | None = None  # puck drop (UTC)
    team: str                          # NHL abbreviation of the goalie's team
    opponent: str | None = None
    home: bool = False
    goalie_name: str
    dfo_id: int | None = None
    strength: str | None = None        # Confirmed / Likely / Expected / Unconfirmed / None
    source: str | None = None          # who reported it
    created_at: datetime | None = None

    @property
    def is_start(self) -> bool:
        """True when the report names him as the starter (confirmed, likely or expected)."""
        return (self.strength or "").strip().lower() in START_STRENGTHS

    @property
    def is_confirmed(self) -> bool:
        return (self.strength or "").strip().lower() == "confirmed"


# --------------------------------------------------------------------------- fetching

def team_url(team_abbrev: str) -> str:
    team = normalize_team(team_abbrev) or ""
    slug = TEAM_SLUGS.get(team)
    if slug is None:
        raise DfoError(f"unknown team {team_abbrev!r}")
    return f"{BASE_URL}/teams/{slug}/line-combinations"


def goalies_url(day: date) -> str:
    return f"{BASE_URL}/starting-goalies/{day.isoformat()}"


def ttl_for(url: str) -> float:
    return TTL_GOALIES if "/starting-goalies/" in url else TTL_LINES


def default_fetch_text(url: str, params: dict | None = None) -> str:
    resp = httpx.get(url, params=params, follow_redirects=True, timeout=20.0,
                     headers={"User-Agent": USER_AGENT})
    resp.raise_for_status()
    return resp.text


def make_cached_fetch_text(cache: Any) -> FetchText:
    """Fetch through ``HttpCache.get_text`` with the Daily Faceoff TTLs and User-Agent."""
    def fetch(url: str, params: dict | None = None) -> str:
        return cache.get_text(url, params=params, headers={"User-Agent": USER_AGENT}, ttl=ttl_for(url))
    return fetch


# --------------------------------------------------------------------------- parsing helpers

def extract_next_data(html: str) -> Any:
    m = _NEXT_DATA_RE.search(html or "")
    if not m:
        raise DfoError("no __NEXT_DATA__ script on the page")
    try:
        return json.loads(m.group(1))
    except ValueError as e:
        raise DfoError(f"__NEXT_DATA__ is not valid JSON ({e})") from e


def walk_dicts(obj: Any) -> Iterator[dict]:
    """Every dict in a JSON tree (depth first, document order)."""
    stack = [obj]
    while stack:
        cur = stack.pop()
        if isinstance(cur, dict):
            yield cur
            stack.extend(reversed(list(cur.values())))
        elif isinstance(cur, list):
            stack.extend(reversed(cur))


def _parse_dt(value: Any) -> datetime | None:
    if not value or not isinstance(value, str):
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None


def _int(v: Any) -> int | None:
    try:
        return int(v) if v is not None and v != "" else None
    except (TypeError, ValueError):
        return None


def _toi_per_game(split: Any) -> float | None:
    if not isinstance(split, dict):
        return None
    gp = _int(split.get("gamesPlayed"))
    if not gp:
        return None
    mins = split.get("toiMinutes", split.get("goalieMinutes"))
    secs = split.get("toiSeconds", split.get("goalieSeconds")) or 0
    if mins is None:
        return None
    try:
        return round((float(mins) + float(secs) / 60.0) / gp, 2)
    except (TypeError, ValueError):
        return None


def _split_val(split: Any, key: str) -> int | None:
    return _int(split.get(key)) if isinstance(split, dict) else None


def _team_from(abbrev: Any = None, slug: Any = None) -> str | None:
    if isinstance(slug, str) and slug in SLUG_TO_TEAM:
        return SLUG_TO_TEAM[slug]
    if isinstance(abbrev, str) and abbrev.strip():
        return normalize_team(abbrev)
    return None


def _is_line_entry(x: Any) -> bool:
    return isinstance(x, dict) and "groupIdentifier" in x and "name" in x


# --------------------------------------------------------------------------- line pages

def parse_team_lines(html_or_data: str | Any, team: str | None = None) -> TeamLines:
    """Parse a team line-combinations page (HTML or its ``__NEXT_DATA__`` JSON)."""
    data = extract_next_data(html_or_data) if isinstance(html_or_data, str) else html_or_data
    holder = next((d for d in walk_dicts(data)
                   if isinstance(d.get("players"), list) and any(_is_line_entry(x) for x in d["players"])), None)
    if holder is None:
        raise DfoError("no line-combination players in __NEXT_DATA__")
    abbrev = _team_from(holder.get("teamAbbreviation"), holder.get("teamSlug")) or normalize_team(team)
    if not abbrev:
        raise DfoError("line page has no team")
    merged: dict[Any, LinePlayer] = {}
    for e in holder["players"]:
        if not _is_line_entry(e):
            continue
        group = str(e.get("groupIdentifier") or "").strip().lower()
        name = str(e.get("name") or "").strip()
        if not group or not name:
            continue
        dfo_id = _int(e.get("playerId"))
        key = dfo_id if dfo_id is not None else name.lower()
        lp = merged.get(key)
        if lp is None:
            lp = merged[key] = LinePlayer(name=name, dfo_id=dfo_id, fantasydata_id=_int(e.get("fantasydataId")),
                                          group=group)
        if group not in lp.groups:
            lp.groups.append(group)
        pos_id = str(e.get("positionIdentifier") or "").strip().lower()
        if group in EV_GROUPS:
            lp.line = lp.line or group
            lp.group = lp.line
            if group == "g":
                lp.position = "G"
                m = re.match(r"g(\d)", pos_id)
                lp.goalie_depth = int(m.group(1)) if m else lp.goalie_depth
            else:
                lp.position = _POS.get(pos_id, lp.position)
        elif group in PP_GROUPS:
            lp.pp_unit = lp.pp_unit or group
        elif group in PK_GROUPS:
            lp.pk_unit = lp.pk_unit or group
        if lp.line is None and group == "ir":
            lp.group = "ir"
        status = e.get("injuryStatus")
        if isinstance(status, str) and status.strip():
            lp.injury_status = status.strip().lower()
        lp.gtd = lp.gtd or bool(e.get("gameTimeDecision"))
        for n, attr in ((5, "last5"), (10, "last10")):
            split = e.get(attr)
            if isinstance(split, dict):
                if getattr(lp, f"toi_last{n}") is None:
                    setattr(lp, f"toi_last{n}", _toi_per_game(split))
                if getattr(lp, f"ppp_last{n}") is None:
                    setattr(lp, f"ppp_last{n}", _split_val(split, "powerplayPoints"))
                if getattr(lp, f"gp_last{n}") is None:
                    setattr(lp, f"gp_last{n}", _split_val(split, "gamesPlayed"))
    # primary group: even-strength line, else ir, else the first group listed
    for lp in merged.values():
        if lp.line is None and "ir" not in lp.groups:
            lp.group = lp.groups[0]
    return TeamLines(team=abbrev, slug=holder.get("teamSlug") or TEAM_SLUGS.get(abbrev),
                     updated_at=_parse_dt(holder.get("updatedAt")),
                     source=(holder.get("sourceName") or None), source_url=(holder.get("source") or None),
                     players=list(merged.values()))


def parse_team_slugs(html_or_data: str | Any) -> dict[str, str]:
    """{NHL abbreviation: slug} from any page listing teams (``sortedTeams`` / ``slug`` +
    ``shortName`` dicts) - used to re-verify :data:`TEAM_SLUGS`."""
    data = extract_next_data(html_or_data) if isinstance(html_or_data, str) else html_or_data
    out: dict[str, str] = {}
    for d in walk_dicts(data):
        slug, short = d.get("slug"), d.get("shortName")
        if isinstance(slug, str) and isinstance(short, str):
            team = normalize_team(short)
            if team:
                out.setdefault(team, slug)
    return out


# --------------------------------------------------------------------------- starting goalies

def _is_goalie_game(d: dict) -> bool:
    return "homeGoalieName" in d and "awayGoalieName" in d


def parse_starting_goalies(html_or_data: str | Any) -> list[GoalieStart]:
    """Both goalies of every game on a starting-goalies page."""
    data = extract_next_data(html_or_data) if isinstance(html_or_data, str) else html_or_data
    games = [d for d in walk_dicts(data) if _is_goalie_game(d)]
    if not games and not any("data" in d for d in walk_dicts(data)):
        raise DfoError("no starting-goalie games in __NEXT_DATA__")
    out: list[GoalieStart] = []
    for g in games:
        teams = {s: _team_from(g.get(f"{s}TeamAbbreviation"), g.get(f"{s}TeamSlug")) for s in ("home", "away")}
        if not teams["home"] or not teams["away"]:
            log.info("dailyfaceoff: skipping game with unknown teams (%s / %s)",
                     g.get("homeTeamSlug"), g.get("awayTeamSlug"))
            continue
        label = f"{teams['away']}@{teams['home']}"
        when = _parse_dt(g.get("dateGmt"))
        for side, other in (("home", "away"), ("away", "home")):
            name = str(g.get(f"{side}GoalieName") or "").strip()
            if not name:
                continue
            strength = g.get(f"{side}NewsStrengthName")
            out.append(GoalieStart(
                game=label, game_time=when, team=teams[side], opponent=teams[other], home=side == "home",
                goalie_name=name, dfo_id=_int(g.get(f"{side}GoalieId")),
                strength=str(strength).strip() if strength else None,
                source=(g.get(f"{side}NewsSourceName") or None),
                created_at=_parse_dt(g.get(f"{side}NewsCreatedAt"))))
    return out


# --------------------------------------------------------------------------- client

class DailyFaceoffClient:
    """Fetch + parse Daily Faceoff pages. ``fetch_text(url, params) -> str`` is injectable
    (tests, or :func:`make_cached_fetch_text` for the HTTP cache)."""

    def __init__(self, fetch_text: FetchText | None = None):
        self.fetch_text = fetch_text or default_fetch_text
        self.warnings: list[str] = []

    def team_lines(self, team_abbrev: str) -> TeamLines:
        url = team_url(team_abbrev)
        html = self.fetch_text(url, None)
        return parse_team_lines(html, team=normalize_team(team_abbrev))

    def all_lines(self, teams: Iterable[str] | None = None) -> dict[str, TeamLines]:
        """{team: lines} for every team (32 fetches); a failing team becomes a warning."""
        out: dict[str, TeamLines] = {}
        failed: list[str] = []
        for t in (list(teams) if teams is not None else list(TEAM_SLUGS)):
            team = normalize_team(t) or t
            try:
                out[team] = self.team_lines(team)
            except Exception as e:  # one team's page must not sink the rest
                failed.append(team)
                log.warning("dailyfaceoff: %s lines unavailable: %s", team, e)
                last = e
        if failed:
            self.warnings.append(f"Daily Faceoff lines unavailable for {len(failed)} team(s) "
                                 f"({', '.join(failed[:6])}{'...' if len(failed) > 6 else ''}): {_short(last)}")
        return out

    def starting_goalies(self, day: date) -> list[GoalieStart]:
        return parse_starting_goalies(self.fetch_text(goalies_url(day), None))


def _short(e: Exception) -> str:
    s = str(e).strip().splitlines()[0] if str(e).strip() else type(e).__name__
    return s[:120]


__all__ = ["DailyFaceoffClient", "DfoError", "GoalieStart", "LinePlayer", "TeamLines", "TEAM_SLUGS",
           "SLUG_TO_TEAM", "TTL_GOALIES", "TTL_LINES", "USER_AGENT", "make_cached_fetch_text",
           "parse_starting_goalies", "parse_team_lines", "parse_team_slugs", "extract_next_data"]
