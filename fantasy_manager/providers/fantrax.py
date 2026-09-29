"""Fantrax league provider.

Talks to Fantrax's private JSON endpoint (``POST https://www.fantrax.com/fxpa/req``) with a
logged-in cookie session, using the same message format as ``fantraxapi`` (``Method`` /
``msgs``) and its exception types. Responses are parsed here directly instead of through
``fantraxapi.League``: the library's constructor makes five calls and parses scoring-period
lists that are fragile in the preseason, and its ``Roster`` parser keeps only the FPts and
FP/G columns. Parsing the raw tables keeps every stat column (G, A, SOG, HIT, ..., GP, Age).

Response shapes are taken from the ``fantraxapi`` 1.0.1 parsers and from the Go client
``github.com/pmurley/go-fantrax`` (``getPlayerStats`` payload/response, roster ``statusId``
codes, ``myTeamIds``, icon type ids).

Stat views: the default roster / player tables are "Projected - Per Game" for the current
scoring period (their GP column is games in the period), so the season-to-date and full-season
projection timeframes are requested explicitly (``seasonOrProjection`` / ``timeframeTypeCode``)
and become the ``season`` and ``projected`` StatLines. The free-agent pool needs separate
skater (``HOCKEY_SKATING``) and goalie (``POS_<id>``) ``positionOrGroup`` queries. Scoring,
roster limits and dynasty rules come from the league Rules page (``getLeagueRulesOld``, HTML),
the method the Fantrax web app calls for /league/<id>/rules.

All responses are cached as JSON files under ``$FM_DATA_DIR/fantrax_cache`` (15 min TTL);
``FM_OFFLINE=1`` serves only from that cache. Cookie values are never logged or cached.

Session: ``fantrax_auth.FantraxAuth`` picks the saved session (``$FM_DATA_DIR/fantrax_session.json``),
else FANTRAX_COOKIE / FANTRAX_COOKIE_FILE, else logs in with FANTRAX_USERNAME / FANTRAX_PASSWORD.
When Fantrax answers WARNING_NOT_LOGGED_IN, the request is retried once after a fresh login.
"""
from __future__ import annotations

import hashlib
import json
import math
import re
import time
from dataclasses import dataclass, field
from datetime import date, datetime
from pathlib import Path
from typing import Any, Callable, Iterable, Mapping

from pydantic import BaseModel, Field

from ..cache import CacheMiss, HttpCache
from ..config import Settings, default_espn_year, format_points, secret_value, split_points
from ..matching.normalize import normalize_team
from ..models import (CANONICAL_STATS, GOALIE_STATS, SKATER_STATS, ActivityItem, FantasyTeam, LeagueContext, LineupDay,
                      Player, RosterSlot,
                      ScoringConfig, StatLine, normalize_name)
from ..scoring import PointsScoring
from .base import ProviderError

FXPA_URL = "https://www.fantrax.com/fxpa/req"
FANTRAX_TTL = 15 * 60
BROWSER_UA = ("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
              "(KHTML, like Gecko) Chrome/126.0 Safari/537.36")
FA_PAGE_SIZE = 200
FA_MAX_PAGES = 3
TAKEN_MAX_PAGES = 2
FA_GOALIE_PAGES = 1
FIT_HINT_MAE = 0.1  # FP/G error above which `fm settings` suggests --fit-points
FPTS_KEY = "FPTS"  # Fantrax's own total fantasy points, stored in the season StatLine

# Roster row statusId -> slot for non-active rows (go-fantrax mapStatusID).
STATUS_SLOTS = {"2": "BN", "3": "IR", "9": "MIN"}
NON_STARTING = frozenset({"BN", "IR", "MIN", "TAXI"})

# Fantrax lineup-slot short names -> canonical slot names. Unknown names pass through upper-cased.
SLOT_ALIASES = {
    "C": "C", "LW": "LW", "RW": "RW", "W": "W", "F": "F", "FWD": "F", "D": "D", "G": "G",
    "UTIL": "UTIL", "UT": "UTIL", "U": "UTIL", "SKT": "UTIL", "SK": "UTIL", "SKATER": "UTIL",
    "FLEX": "UTIL", "RES": "BN", "RESERVE": "BN", "BN": "BN", "BENCH": "BN",
    "IR": "IR", "INJ RES": "IR", "INJ": "IR", "IR+": "IR",
    "MIN": "MIN", "MINORS": "MIN", "MINOR": "MIN", "TAXI": "TAXI",
}

# Player icon typeIds (fantraxapi Player / go-fantrax PlayerIcon).
ICON_DTD, ICON_IR, ICON_OUT, ICON_SUSP = "1", "2", "30", "6"

# Table header shortName (upper-cased) -> canonical stat key.
STAT_COLUMNS = {
    "GP": "GP", "G": "G", "A": "A", "PTS": "PTS", "P": "PTS", "+/-": "PM", "PM": "PM",
    "PIM": "PIM", "PPG": "PPG", "PPA": "PPA", "PPP": "PPP", "SHG": "SHG", "SHA": "SHA",
    "SHP": "SHP", "GWG": "GWG", "FOW": "FOW", "FOL": "FOL", "SOG": "SOG", "SHOTS": "SOG",
    "HIT": "HIT", "HITS": "HIT", "BLK": "BLK", "BKS": "BLK", "BLKS": "BLK", "HAT": "HAT",
    "GS": "GS", "W": "W", "L": "L", "OTL": "OTL", "OTL/SOL": "OTL", "SA": "SA", "GA": "GA",
    "SV": "SV", "SVS": "SV", "SO": "SO", "SHO": "SO", "GAA": "GAA", "SV%": "SVPCT",
    "SVPCT": "SVPCT", "ENG": "ENG", "FT": "FT", "FIGHTS": "FT",
}
FPTS_IDS = ("SCORE", "fpts", "FPTS", "FPts")
FPG_IDS = ("FPTS_PER_GAME", "fptsPerGame", "FP/G")
AGE_IDS = ("age", "AGE", "Age")
OWNED_IDS = ("OVERVIEW_PERCENT_OWNED_2", "Ros", "%Ros", "Own", "%Own")
STATUS_IDS = ("status", "STATUS", "Sta")

# Timeframes ("seasonOrProjection") -> StatLine split. The default roster / player views
# are "Projected - Per Game" for the current scoring period (timeframe PROJECTED_WEEKLY):
# every stat, FPts and the GP column there are projections for the period's games only,
# so they are never used as a season line. Season-to-date and full-season projection
# views are requested explicitly (codes from the response's seasonOrProjections list).
SPLIT_TIMEFRAMES = ("season", "projected")
SKATER_GROUP = "HOCKEY_SKATING"
GOALIE_GROUP_DEFAULT = "POS_201"

# Keywords that flag dynasty / roster-rule settings in the raw league info.
RULE_KEYWORDS = re.compile(
    r"keeper|minor|prospect|salary|contract|draft|dynasty|taxi|rookie|roster|reserve|"
    r"injur|active|max|min(imum)?|limit|claim|waiver|trade|pick|budget|year", re.I)
MAX_RULE_LINES = 60

_TAG_RE = re.compile(r"<[^>]*>")
CANONICAL_STATS_ORDER = tuple(dict.fromkeys(SKATER_STATS + GOALIE_STATS))


# -- small models exposed on the provider -----------------------------------

class TradeBlockInfo(BaseModel):
    team_id: str
    team_name: str
    note: str = ""
    players_offered: list[str] = Field(default_factory=list)   # player cids
    players_wanted: list[str] = Field(default_factory=list)
    positions_offered: list[str] = Field(default_factory=list)
    positions_wanted: list[str] = Field(default_factory=list)
    stats_offered: list[str] = Field(default_factory=list)
    stats_wanted: list[str] = Field(default_factory=list)


class TransactionInfo(BaseModel):
    tx_id: str
    team_id: str | None
    when: str | None
    kind: str
    player_cid: str | None
    player_name: str | None


class PendingTradeInfo(BaseModel):
    trade_id: str
    proposed_by: str | None
    moves: list[dict[str, Any]] = Field(default_factory=list)
    proposed_at: str | None = None     # display text of the "Proposed" info, e.g. "Sep 21, 10:15 AM EDT"


# -- cookies ----------------------------------------------------------------

def parse_cookie_header(raw: str) -> dict[str, str]:
    """'Cookie: a=b; c=d' (prefix optional) -> {'a': 'b', 'c': 'd'}."""
    s = raw.strip()
    if s.lower().startswith("cookie:"):
        s = s[len("cookie:"):]
    out: dict[str, str] = {}
    for part in s.replace("\n", ";").split(";"):
        part = part.strip()
        if not part or "=" not in part:
            continue
        k, v = part.split("=", 1)
        k, v = k.strip(), v.strip()
        if len(v) >= 2 and v[0] == v[-1] == '"':
            v = v[1:-1]
        if k:
            out[k] = v
    return out


def parse_netscape_cookies(text: str, domain: str = "fantrax.com") -> dict[str, str]:
    """Netscape cookies.txt (tab separated, 7 fields) -> {name: value} for `domain`."""
    out: dict[str, str] = {}
    for line in text.splitlines():
        line = line.strip()
        if line.startswith("#HttpOnly_"):
            line = line[len("#HttpOnly_"):]
        elif not line or line.startswith("#"):
            continue
        fields = line.split("\t")
        if len(fields) < 7:
            continue
        if domain not in fields[0].lower():
            continue
        out[fields[5]] = fields[6]
    return out


def _looks_netscape(text: str) -> bool:
    if "Netscape HTTP Cookie File" in text:
        return True
    return any(len(ln.split("\t")) >= 7 for ln in text.splitlines() if ln and not ln.startswith("# "))


def parse_cookie_text(text: str) -> dict[str, str]:
    """Cookie file contents: Netscape cookies.txt, a JSON cookie export, or a header string."""
    t = text.strip().lstrip("﻿")
    if not t:
        return {}
    if t[0] in "[{":
        try:
            data = json.loads(t)
        except ValueError:
            data = None
        if isinstance(data, dict) and "cookies" in data:
            data = data["cookies"]
        if isinstance(data, list):
            return {str(c["name"]): str(c.get("value", "")) for c in data
                    if isinstance(c, dict) and "name" in c
                    and "fantrax" in str(c.get("domain", "fantrax.com")).lower()}
        if isinstance(data, dict):
            return {str(k): str(v) for k, v in data.items()}
    if _looks_netscape(t):
        return parse_netscape_cookies(t)
    return parse_cookie_header(t)


def load_cookies(settings: Settings) -> dict[str, str]:
    """Cookies from FANTRAX_COOKIE, else FANTRAX_COOKIE_FILE. Empty dict if neither is set."""
    raw = secret_value(settings.fantrax_cookie)
    if raw:
        return parse_cookie_header(raw)
    path = settings.fantrax_cookie_file
    if path:
        p = Path(path).expanduser()
        if not p.is_file():
            raise ProviderError(f"FANTRAX_COOKIE_FILE points to {p}, which does not exist.")
        cookies = parse_cookie_text(p.read_text(encoding="utf-8", errors="replace"))
        if not cookies:
            raise ProviderError(f"No fantrax.com cookies found in FANTRAX_COOKIE_FILE ({p}). "
                                "Use a Cookie header string or a Netscape cookies.txt export.")
        return cookies
    return {}


def build_session(cookies: Mapping[str, str]) -> Any:
    import requests

    s = requests.Session()
    s.headers.update({"User-Agent": BROWSER_UA, "Accept": "application/json, text/plain, */*",
                      "Origin": "https://www.fantrax.com", "Referer": "https://www.fantrax.com/"})
    for k, v in cookies.items():
        s.cookies.set(k, v, domain=".fantrax.com", path="/")
    return s


# -- raw fxpa client with a JSON file cache ---------------------------------

def _msg_block(league_id: str, method: str, data: Mapping[str, Any]) -> dict[str, Any]:
    from fantraxapi.api import Method

    return Method(method, **dict(data)).msg_block(league_id)


def _raise_page_error(page_error: Mapping[str, Any]) -> None:
    from fantraxapi.exceptions import FantraxException, NotLoggedIn, NotMemberOfLeague

    code = page_error.get("code")
    if code == "WARNING_NOT_LOGGED_IN":
        raise NotLoggedIn("Not logged in")
    if code == "NOT_MEMBER_OF_LEAGUE":
        raise NotMemberOfLeague("Not member of league")
    raise FantraxException(str(page_error.get("title") or code or page_error))


class FxpaClient:
    """POSTs batched fxpa messages; caches successful responses as JSON files.

    ``relogin`` (optional) is called when Fantrax answers WARNING_NOT_LOGGED_IN: it returns a new
    session to retry the request with once, or None to give up (NotLoggedIn is re-raised)."""

    def __init__(self, league_id: str, session: Any, cache_dir: Path | None,
                 offline: bool = False, ttl: float = FANTRAX_TTL,
                 relogin: Callable[[], Any] | None = None):
        self.league_id = league_id
        self.session = session
        self.cache_dir = Path(cache_dir) if cache_dir else None
        self.offline = offline
        self.ttl = ttl
        self.relogin = relogin
        self.requests_made = 0

    def _key(self, msgs: list[dict]) -> str:
        blob = json.dumps({"league": self.league_id, "msgs": msgs}, sort_keys=True)
        return hashlib.sha256(blob.encode("utf-8")).hexdigest()

    def _cached_post(self, msgs: list[dict], fresh: bool = False) -> dict:
        key = self._key(msgs)
        path = self.cache_dir / f"{key}.json" if self.cache_dir else None
        if path is not None and path.is_file() and not (fresh and not self.offline):
            try:
                entry = json.loads(path.read_text(encoding="utf-8"))
                if self.offline or time.time() - float(entry["fetched_at"]) < self.ttl:
                    return entry["body"]
            except (ValueError, KeyError, OSError):
                pass
        if self.offline:
            raise CacheMiss(f"offline and Fantrax request not cached: {[m['method'] for m in msgs]}")
        body = self._post(msgs)
        if path is not None:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps({"fetched_at": time.time(), "body": body}), encoding="utf-8")
        return body

    def _post(self, msgs: list[dict]) -> dict:
        from fantraxapi.exceptions import NotLoggedIn

        try:
            return self._post_once(msgs)
        except NotLoggedIn:
            if self.relogin is None:
                raise
            session = self.relogin()
            if session is None:
                raise
            self.session = session
            return self._post_once(msgs)

    def _post_once(self, msgs: list[dict]) -> dict:
        from fantraxapi.exceptions import FantraxException

        payload = {"msgs": msgs, "uiv": 3, "refUrl": f"https://www.fantrax.com/fantasy/league/{self.league_id}",
                   "dt": 0, "at": 0, "av": "0.0", "tz": "UTC"}
        self.requests_made += 1
        resp = self.session.post(FXPA_URL, params={"leagueId": self.league_id}, json=payload, timeout=30)
        status = getattr(resp, "status_code", 200)
        try:
            body = resp.json()
        except ValueError as e:
            raise FantraxException(f"Fantrax returned a non-JSON response (HTTP {status})") from e
        if isinstance(body, dict) and isinstance(body.get("pageError"), dict):
            _raise_page_error(body["pageError"])
        if status >= 400:
            raise FantraxException(f"Fantrax HTTP {status}")
        if not isinstance(body, dict) or not isinstance(body.get("responses"), list):
            raise FantraxException("Fantrax response has no 'responses' list")
        return body

    def call(self, *calls: tuple[str, Mapping[str, Any]], fresh: bool = False) -> list[dict]:
        """call(("getStandings", {}), ...) -> one `data` dict per message. ``fresh`` skips the cache."""
        msgs = [_msg_block(self.league_id, m, d) for m, d in calls]
        body = self._cached_post(msgs, fresh=fresh)
        out = [r.get("data", {}) if isinstance(r, dict) else {} for r in body["responses"]]
        if len(out) != len(msgs):
            from fantraxapi.exceptions import FantraxException
            raise FantraxException(f"expected {len(msgs)} responses, got {len(out)}")
        return out


# -- pure parsing helpers ---------------------------------------------------

def strip_html(s: Any) -> str:
    return _TAG_RE.sub("", str(s or "")).strip()


def _num(content: Any) -> float | None:
    if content is None:
        return None
    s = strip_html(content).replace(",", "").replace("%", "").strip()
    if s in ("", "-", "--", "N/A"):
        return None
    if s.startswith("+"):
        s = s[1:]
    if s.startswith("."):
        s = "0" + s
    try:
        return float(s)
    except ValueError:
        return None


def canonical_slot(short_name: str | None) -> str:
    s = strip_html(short_name).upper()
    return SLOT_ALIASES.get(s, s or "BN")


def player_positions(pos_short_names: str | None) -> list[str]:
    """'<b>C</b>,RW' -> ['C', 'RW', 'F'] (F added for forwards, like the ESPN provider)."""
    order = ["C", "LW", "RW", "D", "G"]
    found: set[str] = set()
    for part in re.split(r"[,/\s]+", strip_html(pos_short_names).upper()):
        if part in ("L",):
            part = "LW"
        elif part in ("R",):
            part = "RW"
        if part == "W":
            found |= {"LW", "RW"}
        elif part == "F":
            found.add("F")
        elif part in order:
            found.add(part)
    pos = [p for p in order if p in found]
    if found & {"C", "LW", "RW", "F"}:
        pos.insert(len([p for p in pos if p in ("C", "LW", "RW")]), "F")
    return pos


def status_from_icons(icons: Iterable[Mapping[str, Any]] | None) -> tuple[str, str | None]:
    ids: dict[str, str] = {}
    for icon in icons or []:
        tid = str(icon.get("typeId", ""))
        ids.setdefault(tid, strip_html(icon.get("tooltip") or icon.get("toolTip") or ""))
    for tid, status in ((ICON_IR, "ir"), (ICON_OUT, "out"), (ICON_SUSP, "suspended"), (ICON_DTD, "dtd")):
        if tid in ids:
            return status, ids[tid] or None
    return "healthy", None


def fantrax_team_abbrev(short: str | None) -> str | None:
    s = strip_html(short).upper()
    if not s or s in ("FA", "(N/A)", "N/A", "NA", "-", "FREE AGENT"):
        return None
    return normalize_team(s)


def header_index(cells: Iterable[Mapping[str, Any]]) -> dict[str, int]:
    """Index header cells by every identifier Fantrax exposes (key, sortKey, sortType, shortName)."""
    idx: dict[str, int] = {}
    for i, c in enumerate(cells or []):
        for f in ("key", "sortKey", "sortType", "shortName"):
            v = c.get(f)
            if v:
                idx.setdefault(str(v), i)
    return idx


def _find(idx: Mapping[str, int], ids: Iterable[str]) -> int | None:
    for i in ids:
        if i in idx:
            return idx[i]
    return None


def stat_columns(header_cells: list[Mapping[str, Any]]) -> dict[int, str]:
    """Column index -> canonical stat key, by header shortName."""
    out: dict[int, str] = {}
    for i, c in enumerate(header_cells or []):
        ident = f"{c.get('sortType', '')} {c.get('sortKey', '')}".upper()
        if "PERCENT" in ident or "OWNED" in ident:  # e.g. player pool "+/-" = roster % change
            continue
        key = STAT_COLUMNS.get(strip_html(c.get("shortName")).upper())
        if key and key in CANONICAL_STATS and key not in out.values():
            out[i] = key
    return out


@dataclass
class RowStats:
    stats: dict[str, float] = field(default_factory=dict)
    fpts: float | None = None
    fpg: float | None = None
    age: float | None = None
    owned: float | None = None
    status_cell: str | None = None
    status_team_id: str | None = None


def parse_row_cells(header_cells: list[Mapping[str, Any]], cells: list[Mapping[str, Any]]) -> RowStats:
    idx = header_index(header_cells)
    out = RowStats()

    def cell(i: int | None) -> Mapping[str, Any] | None:
        if i is None or i >= len(cells):
            return None
        return cells[i]

    for i, key in stat_columns(header_cells).items():
        c = cell(i)
        v = _num(c.get("content")) if c else None
        if v is not None:
            out.stats[key] = v
    if (c := cell(_find(idx, FPTS_IDS))) is not None:
        out.fpts = _num(c.get("content"))
    if (c := cell(_find(idx, FPG_IDS))) is not None:
        out.fpg = _num(c.get("content"))
    if (c := cell(_find(idx, AGE_IDS))) is not None:
        out.age = _num(c.get("content"))
    if (c := cell(_find(idx, OWNED_IDS))) is not None:
        out.owned = _num(c.get("content"))
    if (c := cell(_find(idx, STATUS_IDS))) is not None:
        out.status_cell = strip_html(c.get("content")) or None
        out.status_team_id = c.get("teamId")
    return out


def season_line(rs: RowStats, split: str = "season") -> StatLine | None:
    """StatLine (split `split`) from a table row; GP from the GP column, else FPts / FP/G.

    Only call this for season-to-date / full-season views: in the default per-period
    projection view the GP column is the number of games in the scoring period."""
    stats = dict(rs.stats)
    gp = stats.get("GP")
    if not gp and rs.fpts is not None and rs.fpg:
        gp = round(rs.fpts / rs.fpg)
        if gp > 0:
            stats["GP"] = float(gp)
    if not gp or gp <= 0:
        return None
    if "PTS" not in stats and ("G" in stats or "A" in stats) and "W" not in stats:
        stats["PTS"] = stats.get("G", 0.0) + stats.get("A", 0.0)
    if rs.fpts is not None:
        stats[FPTS_KEY] = rs.fpts
    return StatLine(split=split, gp=int(round(gp)), stats=stats)  # type: ignore[arg-type]


def player_from_scorer(scorer: Mapping[str, Any], rs: RowStats | None = None,
                       split: str | None = "season") -> Player:
    sid = str(scorer["scorerId"])
    name = strip_html(scorer.get("name")) or sid
    status, note = status_from_icons(scorer.get("icons"))
    lines: dict[str, StatLine] = {}
    if rs is not None and split and (ln := season_line(rs, split)) is not None:
        lines[split] = ln
    return Player(
        cid=f"fantrax:{sid}",
        name=name,
        name_norm=normalize_name(name),
        ids={"fantrax": sid},
        team=fantrax_team_abbrev(scorer.get("teamShortName") or scorer.get("teamName")),
        positions=player_positions(scorer.get("posShortNames")),
        status=status,
        status_note=note,
        lines=lines,
        pct_owned=rs.owned if rs is not None else None,
    )


@dataclass
class ParsedRoster:
    slots: list[RosterSlot]
    fantasy_points: dict[str, tuple[float | None, float | None]]
    ages: dict[str, float]
    limits: dict[str, tuple[int, int]]           # status name -> (total, max)
    slot_counts: dict[str, int]                  # starting slot -> count (incl. empty)
    split: str | None = "season"                 # StatLine split of this view (None: period view)


def parse_status_totals(data: Mapping[str, Any]) -> dict[str, tuple[int, int]]:
    out: dict[str, tuple[int, int]] = {}
    totals = (data.get("miscData") or {}).get("statusTotals") or []
    for t in totals:
        name = str(t.get("name") or t.get("statusId") or "")
        if not name:
            continue
        try:
            out[name] = (int(t.get("total") or 0), int(t.get("max") or 0))
        except (TypeError, ValueError):
            continue
    return out


def parse_roster(data: Mapping[str, Any], positions: Mapping[str, str],
                 codes: Mapping[str, Mapping[str, str]] | None = None) -> ParsedRoster:
    """Parse a getTeamRosterInfo (view=STATS) response. `positions` maps posId -> shortName.

    Stat lines get the split of the displayed timeframe (see `data_split`); rows of the
    default per-period projection view get no stat line at all."""
    split = data_split(data, codes)
    slots: list[RosterSlot] = []
    fp: dict[str, tuple[float | None, float | None]] = {}
    ages: dict[str, float] = {}
    counts: dict[str, int] = {}
    for table in data.get("tables") or []:
        header = (table.get("header") or {}).get("cells") or []
        for row in table.get("rows") or []:
            if "posId" not in row and "scorer" not in row:
                continue
            status_id = str(row.get("statusId", "1"))
            slot = STATUS_SLOTS.get(status_id) or canonical_slot(positions.get(str(row.get("posId")), ""))
            starting = slot not in NON_STARTING
            scorer = row.get("scorer")
            if not scorer or not scorer.get("scorerId"):
                if status_id != "1":
                    continue  # empty reserve rows carry no information
                slots.append(RosterSlot(slot=slot, player=None, starting=starting))
                counts[slot] = counts.get(slot, 0) + 1
                continue
            rs = parse_row_cells(header, row.get("cells") or [])
            p = player_from_scorer(scorer, rs, split)
            if split:
                fp[p.cid] = (rs.fpts, rs.fpg)
            if rs.age is not None:
                ages[p.cid] = rs.age
            slots.append(RosterSlot(slot=slot, player=p, starting=starting))
            if starting:
                counts[slot] = counts.get(slot, 0) + 1
    return ParsedRoster(slots, fp, ages, parse_status_totals(data), counts, split)


def roster_shape(slot_counts: Mapping[str, int], limits: Mapping[str, tuple[int, int]]) -> dict[str, int]:
    shape = {k: v for k, v in slot_counts.items() if v > 0}
    for name, slot in (("Reserve", "BN"), ("Inj Res", "IR"), ("Injured Reserve", "IR"), ("Minors", "MIN")):
        if name in limits and limits[name][1] > 0:
            shape[slot] = limits[name][1]
    return shape


def parse_positions(position_map: Mapping[str, Any] | None) -> dict[str, str]:
    out: dict[str, str] = {}
    for pid, v in (position_map or {}).items():
        if isinstance(v, Mapping):
            out[str(v.get("id", pid))] = str(v.get("shortName") or v.get("name") or pid)
    return out


def parse_teams(data: Mapping[str, Any]) -> dict[str, dict[str, str]]:
    """fantasyTeams (list or id->dict) -> {id: {'name':..., 'short':...}}."""
    raw = data.get("fantasyTeams") or {}
    if isinstance(raw, list):
        raw = {str(t.get("id")): t for t in raw if isinstance(t, Mapping)}
    return {str(tid): {"name": str(t.get("name", tid)), "short": str(t.get("shortName", ""))}
            for tid, t in raw.items() if isinstance(t, Mapping)}


def parse_standings(data: Mapping[str, Any]) -> dict[str, dict[str, Any]]:
    """getStandings -> {team_id: {'rank', 'win', 'loss', 'tie', 'points_for', 'points_against'}}."""
    out: dict[str, dict[str, Any]] = {}
    tables = data.get("tableList") or []
    if not tables:
        return out
    table = tables[0]
    fields = {c.get("key"): i for i, c in enumerate((table.get("header") or {}).get("cells") or [])}

    def val(cells: list, key: str, cast=float) -> Any:
        i = fields.get(key)
        if i is None or i >= len(cells):
            return None
        v = _num(cells[i].get("content"))
        return cast(v) if v is not None else None

    for row in table.get("rows") or []:
        fixed = row.get("fixedCells") or []
        team_id = next((c.get("teamId") for c in fixed if c.get("teamId")), None)
        if not team_id:
            continue
        cells = row.get("cells") or []
        rank = _num(fixed[0].get("content")) if fixed else None
        out[str(team_id)] = {
            "rank": int(rank) if rank is not None else None,
            "win": val(cells, "win", int) or 0, "loss": val(cells, "loss", int) or 0,
            "tie": val(cells, "tie", int) or 0,
            "points_for": val(cells, "pointsFor"), "points_against": val(cells, "pointsAgainst"),
            "waiver_order": val(cells, "wwOrder", int),
        }
    return out


def parse_trade_blocks(blocks: Iterable[Mapping[str, Any]], positions: Mapping[str, str],
                       teams: Mapping[str, Mapping[str, str]]) -> list[TradeBlockInfo]:
    out: list[TradeBlockInfo] = []
    for b in blocks or []:
        if not isinstance(b, Mapping) or len(b) <= 2 or "teamId" not in b:
            continue
        tid = str(b["teamId"])

        def scorers(key: str) -> list[str]:
            groups = ((b.get(key) or {}).get("scorers") or {}).values()
            return [f"fantrax:{s['scorerId']}" for g in groups for s in g if s.get("scorerId")]

        def poss(key: str) -> list[str]:
            return [canonical_slot(positions.get(str(p), str(p))) for p in (b.get(key) or {}).get("positions") or []]

        def stats(key: str) -> list[str]:
            return [str(s.get("shortName")) for s in (b.get(key) or {}).get("stats") or [] if s.get("shortName")]

        out.append(TradeBlockInfo(
            team_id=tid, team_name=teams.get(tid, {}).get("name", tid),
            note=strip_html((b.get("comment") or {}).get("body", "")),
            players_offered=scorers("scorersOffered"), players_wanted=scorers("scorersWanted"),
            positions_offered=poss("positionsOffered"), positions_wanted=poss("positionsWanted"),
            stats_offered=stats("statsOffered"), stats_wanted=stats("statsWanted")))
    return out


def parse_transactions(data: Mapping[str, Any]) -> list[TransactionInfo]:
    out: list[TransactionInfo] = []
    for row in ((data.get("table") or {}).get("rows") or []):
        cells = row.get("cells") or []
        scorer = row.get("scorer") or {}
        code = str(row.get("transactionCode") or "")
        kind = str(row.get("claimType") or code) if code == "CLAIM" else code
        out.append(TransactionInfo(
            tx_id=str(row.get("txSetId", "")),
            team_id=(cells[0].get("teamId") if cells else None),
            when=(strip_html(cells[1].get("content")) if len(cells) > 1 else None),
            kind=kind or "UNKNOWN",
            player_cid=f"fantrax:{scorer['scorerId']}" if scorer.get("scorerId") else None,
            player_name=strip_html(scorer.get("name")) or None))
    return out


def parse_pending_trades(data: Mapping[str, Any]) -> list[PendingTradeInfo]:
    out: list[PendingTradeInfo] = []
    for t in data.get("tradeInfoList") or []:
        moves = []
        for m in t.get("moves") or []:
            mv: dict[str, Any] = {"from": (m.get("from") or {}).get("teamId"),
                                  "to": (m.get("to") or {}).get("teamId")}
            if "draftPick" in m:
                dp = m["draftPick"]
                mv["pick"] = {"year": dp.get("year"), "round": dp.get("round"),
                              "original_owner": (dp.get("origOwnerTeam") or {}).get("id")}
            elif m.get("scorer"):
                mv["player_cid"] = f"fantrax:{m['scorer'].get('scorerId')}"
                mv["player_name"] = m["scorer"].get("name")
            moves.append(mv)
        info = {str(i.get("name")): i.get("value") for i in t.get("usefulInfo") or [] if isinstance(i, Mapping)}
        out.append(PendingTradeInfo(trade_id=str(t.get("txSetId", "")),
                                    proposed_by=t.get("creatorTeamId"), moves=moves,
                                    proposed_at=str(info["Proposed"]) if info.get("Proposed") else None))
    return out

# -- activity feed (harness capture) -------------------------------------------

WHEN_FORMATS_WITH_YEAR = ("%a %b %d, %Y, %I:%M%p", "%a %b %d, %Y, %I:%M %p", "%b %d, %Y, %I:%M%p",
                          "%b %d, %Y, %I:%M %p", "%a %b %d, %Y", "%b %d, %Y")
WHEN_FORMATS_NO_YEAR = ("%a %b %d, %I:%M%p", "%a %b %d, %I:%M %p", "%b %d, %I:%M%p", "%b %d, %I:%M %p",
                        "%a %b %d", "%b %d")
_TZ_RE = re.compile(r"\s+(E[DS]T|C[DS]T|M[DS]T|P[DS]T|ET|UTC|GMT)\b", re.I)
FANTRAX_ADD_KINDS = frozenset({"CLAIM", "FA", "WW", "WAIVER", "ADD", "FREE_AGENT"})
FANTRAX_DROP_KINDS = frozenset({"DROP"})
FANTRAX_TRADE_KINDS = frozenset({"TRADE"})
FANTRAX_IR_KINDS = frozenset({"IR", "INJURED_RESERVE", "RESERVE_IR"})
FANTRAX_ACTIVATE_KINDS = frozenset({"ACTIVATE", "ACTIVATED"})


def season_start_year_for(day: date) -> int:
    """Start year of the NHL season ``day`` belongs to (August on = the upcoming season)."""
    return day.year if day.month >= 8 else day.year - 1


def parse_fantrax_when(text: str | None, season_start_year: int | None = None) -> datetime | None:
    """Parse Fantrax's display timestamps ("Mon Sep 21, 2026, 10:15AM"; trade info
    "Sep 21, 10:15 AM EDT"). Without a year the season is inferred like fantraxapi's
    ``Trade._parse_datetime``: the date must fall in the season starting ``season_start_year``
    (Aug..Dec of that year, Jan..Jul of the next). None when unparseable."""
    if not text:
        return None
    t = _TZ_RE.sub("", strip_html(text)).strip().replace(" ", " ")
    t = re.sub(r"\s+", " ", t)
    for fmt in WHEN_FORMATS_WITH_YEAR:
        try:
            return datetime.strptime(t, fmt)
        except ValueError:
            continue
    y0 = season_start_year if season_start_year is not None else season_start_year_for(date.today())
    for fmt in WHEN_FORMATS_NO_YEAR:
        for year in (y0, y0 + 1):
            try:
                d = datetime.strptime(f"{t} {year}", f"{fmt} %Y")
            except ValueError:
                continue
            if (d.month >= 8 and year == y0) or (d.month < 8 and year == y0 + 1):
                return d
    return None


def activity_from_transactions(txs: Iterable[TransactionInfo], team_names: Mapping[str, str] | None = None,
                               season_start_year: int | None = None) -> list[ActivityItem]:
    """TransactionInfo rows -> ActivityItems. Claims are ADDs, drops DROPs. A trade row moves
    the player to the row's team (TRADE_IN); with exactly two teams in the trade the other
    team gets the matching TRADE_OUT. Rows with an unparseable time are skipped."""
    names = dict(team_names or {})
    rows = list(txs)
    trade_teams: dict[str, set[str]] = {}
    for t in rows:
        if t.kind.upper() in FANTRAX_TRADE_KINDS and t.team_id:
            trade_teams.setdefault(t.tx_id, set()).add(t.team_id)
    out: list[ActivityItem] = []
    for t in rows:
        ts = parse_fantrax_when(t.when, season_start_year)
        if ts is None:
            continue
        kind = t.kind.upper()

        def item(action: str, team: str | None, other: str | None = None) -> ActivityItem:
            return ActivityItem(source="fantrax", tx_id=t.tx_id, ts=ts, team_id=team, team_name=names.get(team or ""),
                                action=action, cid=t.player_cid, player_name=t.player_name,  # type: ignore[arg-type]
                                group_id=t.tx_id, counterparty_id=other)

        if kind in FANTRAX_TRADE_KINDS:
            others = sorted((trade_teams.get(t.tx_id) or set()) - {t.team_id})
            other = others[0] if len(others) == 1 else None
            out.append(item("TRADE_IN", t.team_id, other))
            if other is not None:
                out.append(item("TRADE_OUT", other, t.team_id))
        elif kind in FANTRAX_DROP_KINDS:
            out.append(item("DROP", t.team_id))
        elif kind in FANTRAX_IR_KINDS:
            out.append(item("IR", t.team_id))
        elif kind in FANTRAX_ACTIVATE_KINDS:
            out.append(item("ACTIVATE", t.team_id))
        elif kind in FANTRAX_ADD_KINDS or kind.endswith("CLAIM"):
            out.append(item("ADD", t.team_id))
    return out


def activity_from_pending(trades: Iterable[PendingTradeInfo], team_names: Mapping[str, str] | None = None,
                          season_start_year: int | None = None, now: datetime | None = None) -> list[ActivityItem]:
    """Pending trade proposals -> PROPOSED items, one per player move: ``team_id`` is the
    receiving team, ``counterparty_id`` the giving team (draft picks are skipped). The
    timestamp is the trade's "Proposed" info when parseable, else ``now`` (the provider passes
    the start of its as_of day; the ledger keeps the first-seen time of a proposal)."""
    names = dict(team_names or {})
    out: list[ActivityItem] = []
    for tr in trades:
        ts = parse_fantrax_when(tr.proposed_at, season_start_year) or now or datetime.now()
        for mv in tr.moves:
            if not mv.get("player_cid"):
                continue
            to = mv.get("to")
            out.append(ActivityItem(source="fantrax", tx_id=tr.trade_id, ts=ts, team_id=to,
                                    team_name=names.get(to or ""), action="PROPOSED", cid=mv["player_cid"],
                                    player_name=mv.get("player_name"), group_id=tr.trade_id,
                                    counterparty_id=mv.get("from")))
    return out


def lineup_days_from_teams(teams: Iterable[FantasyTeam], day: date) -> list[LineupDay]:
    """Roster snapshot -> LineupDay rows (starting = slot not in BN / IR / MIN / TAXI)."""
    out: list[LineupDay] = []
    for t in teams:
        for sl in t.slots:
            if sl.player is None:
                continue
            out.append(LineupDay(team_id=t.team_id, date=day, cid=sl.player.cid, slot=sl.slot,
                                 starting=sl.slot not in NON_STARTING))
    return out



def parse_player_pool(data: Mapping[str, Any], codes: Mapping[str, Mapping[str, str]] | None = None
                      ) -> tuple[list[Player], dict[str, RowStats], int]:
    """getPlayerStats -> (available players, row stats by cid, total pages).

    Stat lines get the split of the displayed timeframe (see `data_split`)."""
    split = data_split(data, codes)
    header = (data.get("tableHeader") or {}).get("cells") or []
    players: list[Player] = []
    rows: dict[str, RowStats] = {}
    for entry in data.get("statsTable") or []:
        scorer = entry.get("scorer") or {}
        if not scorer.get("scorerId"):
            continue
        rs = parse_row_cells(header, entry.get("cells") or [])
        if rs.status_team_id:  # rostered by a fantasy team: not a free agent
            continue
        p = player_from_scorer(scorer, rs, split)
        players.append(p)
        rows[p.cid] = rs
    pages = int((data.get("paginatedResultSet") or {}).get("totalNumPages") or 1)
    return players, rows, pages


def parse_taken_ownership(data: Mapping[str, Any]) -> dict[str, tuple[float | None, float | None]]:
    """getPlayerStats (statusOrTeamFilter=ALL_TAKEN) -> {cid: (% rostered, age)} for players
    on a fantasy team. The roster view has no "Ros" column, so this is where rostered
    players' ownership comes from."""
    header = (data.get("tableHeader") or {}).get("cells") or []
    out: dict[str, tuple[float | None, float | None]] = {}
    for entry in data.get("statsTable") or []:
        sid = (entry.get("scorer") or {}).get("scorerId")
        if not sid:
            continue
        rs = parse_row_cells(header, entry.get("cells") or [])
        if not rs.status_team_id:
            continue
        out[f"fantrax:{sid}"] = (rs.owned, rs.age)
    return out


# -- timeframes ---------------------------------------------------------------

def displayed_timeframe(data: Mapping[str, Any]) -> Mapping[str, Any] | None:
    """The seasonOrProjection a roster / player-stats response is showing (None if absent)."""
    ds = data.get("displayedSelections") or {}
    tf = ds.get("displayedSeasonOrProjection") if isinstance(ds, Mapping) else None
    tf = tf or data.get("displayedSeasonOrProjection")
    return tf if isinstance(tf, Mapping) else None


def timeframe_options(data: Mapping[str, Any]) -> list[Mapping[str, Any]]:
    lists = data.get("displayedLists") or {}
    for opts in (lists.get("seasonOrProjections"), lists.get("displayedSeasonOrProjections"),
                 data.get("seasonOrProjections")):
        if isinstance(opts, list) and opts:
            return [o for o in opts if isinstance(o, Mapping) and o.get("code")]
    return []


def timeframe_codes(data: Mapping[str, Any]) -> dict[str, dict[str, str]]:
    """{'season'|'prior'|'projected': {'seasonOrProjection', 'timeframeTypeCode'}} from a response.

    season = the latest regular-season year-to-date view, prior = the one before it,
    projected = the full-season projection. Empty when the response lists no timeframes."""
    opts = timeframe_options(data)
    ytd = [o for o in opts if o.get("timeframeTypeCode") == "YEAR_TO_DATE"]
    reg = [o for o in ytd if "REG" in str(o.get("name", "")).upper()] or ytd
    reg.sort(key=lambda o: float(o.get("endDate") or 0), reverse=True)
    chosen: dict[str, Mapping[str, Any]] = {}
    if reg:
        chosen["season"] = reg[0]
    if len(reg) > 1:
        chosen["prior"] = reg[1]
    proj = next((o for o in opts if o.get("timeframeTypeCode") == "PROJECTED_SEASON"), None)
    if proj:
        chosen["projected"] = proj
    return {k: {"seasonOrProjection": str(o["code"]), "timeframeTypeCode": str(o.get("timeframeTypeCode", ""))}
            for k, o in chosen.items()}


def data_split(data: Mapping[str, Any], codes: Mapping[str, Mapping[str, str]] | None = None) -> str | None:
    """StatLine split for a response's rows: 'season' / 'prior' / 'projected', or None for
    per-period views (e.g. the default "Projected - Per Game" view, whose GP column is the
    number of games in the current scoring period). Responses without timeframe info are
    treated as season-to-date."""
    tf = displayed_timeframe(data)
    if tf is None:
        return "season"
    code = str(tf.get("code", ""))
    for split, c in (codes or timeframe_codes(data)).items():
        if c.get("seasonOrProjection") == code:
            return split
    if tf.get("timeframeTypeCode") == "PROJECTED_SEASON":
        return "projected"
    return None


def merge_lines(target: Player, other: Player) -> None:
    """Copy stat lines `target` lacks from `other` (same player, another timeframe)."""
    for split, line in other.lines.items():
        target.lines.setdefault(split, line)
    if target.pct_owned is None and other.pct_owned is not None:
        target.pct_owned = other.pct_owned


def pick_fantasy_points(by_split: Mapping[str, tuple[float | None, float | None]], player: Player
                        ) -> tuple[float | None, float | None] | None:
    """(FPts, FP/G): season-to-date once the player has games, else Fantrax's projection."""
    for split in ("season", "projected", "prior"):
        if split in by_split and split in player.lines:
            return by_split[split]
    return next(iter(by_split.values()), None)


def merge_rosters(parsed: list[ParsedRoster]) -> ParsedRoster:
    """Merge the same roster parsed from several timeframes into the first one."""
    main = parsed[0]
    others: dict[str, Player] = {}
    fp_by: dict[str, dict[str, tuple[float | None, float | None]]] = {}
    for pr in parsed:
        for cid, v in pr.fantasy_points.items():
            fp_by.setdefault(cid, {})[pr.split or "?"] = v
        if pr is main:
            continue
        for sl in pr.slots:
            if sl.player is not None:
                others.setdefault(sl.player.cid, sl.player)
        for cid, age in pr.ages.items():
            main.ages.setdefault(cid, age)
    fp: dict[str, tuple[float | None, float | None]] = {}
    for sl in main.slots:
        p = sl.player
        if p is None:
            continue
        if p.cid in others:
            merge_lines(p, others[p.cid])
        if p.cid in fp_by and (v := pick_fantasy_points(fp_by[p.cid], p)) is not None:
            fp[p.cid] = v
    main.fantasy_points = fp
    return main


def parse_draft_picks(data: Mapping[str, Any]) -> dict[int, list[dict[str, Any]]]:
    """draftPicksData (my roster response) -> {year: [{'round', 'original_owner'}]}."""
    out: dict[int, list[dict[str, Any]]] = {}
    for y in ((data.get("draftPicksData") or {}).get("draftPicksPerYear") or []):
        try:
            year = int(y.get("year"))
        except (TypeError, ValueError):
            continue
        out[year] = [{"round": pk.get("round"), "original_owner": pk.get("origOwnerTeamId")}
                     for pk in y.get("draftPickList") or [] if isinstance(pk, Mapping)]
    return out


# -- league rules page (getLeagueRulesOld) -------------------------------------

_RULE_HEAD_RE = re.compile(r"<h[36][^>]*>(.*?)</h[36]>", re.S | re.I)
_RULE_SPAN_RE = re.compile(r"<span[^>]*>(.*?)</span>", re.S | re.I)
_RULE_VALUE_END = re.compile(r"<span|<li|</li|</ul|<br|<h\d|<div|<table|<b>\s*&bull;", re.I)
_SCORING_ROW_RE = re.compile(
    r'<td class="scGroup">(.*?)</td>\s*<td class="scoringPts">(.*?)</td>\s*<td class="points">(.*?)</td>',
    re.S | re.I)
_ROW_RE = re.compile(r"<tr[^>]*>(.*?)</tr>", re.S | re.I)
_CELL_RE = re.compile(r"<t[dh][^>]*>(.*?)</t[dh]>", re.S | re.I)
_ABBR_RE = re.compile(r"\(([^()]+)\)\s*$")
# Labels not repeated by describe_settings (other managers' identities, ids).
RULE_SKIP_LABELS = frozenset({"League Creator Username", "Commissioner Team Name", "League ID", "Draft Order"})


def _text(html_fragment: str) -> str:
    import html as _html
    return re.sub(r"\s+", " ", _html.unescape(_TAG_RE.sub(" ", html_fragment))).strip()


@dataclass
class LeagueRules:
    """What the league's Rules page says (sections of label -> value, scoring, position limits)."""
    settings: dict[str, dict[str, str]] = field(default_factory=dict)
    scoring: dict[str, dict[str, float]] = field(default_factory=dict)   # group -> {abbr: points}
    scoring_names: dict[str, str] = field(default_factory=dict)          # abbr -> category name
    positions: dict[str, dict[str, int | None]] = field(default_factory=dict)  # pos -> min/max active, max

    def get(self, label: str) -> str | None:
        for sect in self.settings.values():
            for k, v in sect.items():
                if k.lower() == label.lower():
                    return v
        return None

    def get_int(self, label: str) -> int | None:
        m = re.match(r"\s*(-?\d+)", self.get(label) or "")
        return int(m.group(1)) if m else None

    @property
    def empty(self) -> bool:
        return not (self.settings or self.scoring or self.positions)


def parse_league_rules(content: str) -> LeagueRules:
    """Parse the HTML of getLeagueRulesOld (the league's Rules page)."""
    rules = LeagueRules()
    if not content:
        return rules
    heads = [(m.start(), _text(m.group(1))) for m in _RULE_HEAD_RE.finditer(content)]

    def section(pos: int) -> str:
        name = "General"
        for start, title in heads:
            if start > pos:
                break
            name = title or name
        return name

    for m in _RULE_SPAN_RE.finditer(content):
        label = _text(m.group(1)).rstrip(":").strip()
        if not label or len(label) > 120:
            continue
        rest = content[m.end():]
        end = _RULE_VALUE_END.search(rest)
        value = _text(rest[:end.start()] if end else rest[:300])
        rules.settings.setdefault(section(m.start()), {})[label] = value
    for group, cat, pts in _SCORING_ROW_RE.findall(content):
        name, points = _text(cat), _num(_text(pts))
        abbr_m = _ABBR_RE.search(name)
        if not name or points is None or not abbr_m:
            continue
        abbr = abbr_m.group(1).strip()
        rules.scoring.setdefault(_text(group) or "All", {})[abbr] = points
        rules.scoring_names.setdefault(abbr, name)
    for row in _ROW_RE.findall(content):
        cells = [_text(c) for c in _CELL_RE.findall(row)]
        if len(cells) >= 3 and (pm := _ABBR_RE.search(cells[0])) and all(
                re.fullmatch(r"-?\d*", c) for c in cells[1:4]):
            vals = [int(c) if c else None for c in cells[1:4]] + [None] * (3 - len(cells[1:4]))
            rules.positions[pm.group(1).strip().upper()] = {"min_active": vals[0], "max_active": vals[1],
                                                            "max": vals[2]}
    return rules


def stat_key(abbr: str) -> str | None:
    """Fantrax category abbreviation -> canonical stat key (None if not tracked)."""
    key = STAT_COLUMNS.get(abbr.strip().upper())
    return key if key in CANONICAL_STATS else None


def rules_weights(rules: LeagueRules) -> tuple[dict[str, float], dict[str, float], list[str]]:
    """(base weights, goalie-group overrides, notes) from the Rules page scoring table.

    Skater points form the base; goalie-only stats (W, SV, ...) join the base; goalie values
    that differ from the skater value of the same stat (e.g. goalie G) become overrides."""
    base: dict[str, float] = {}
    goalie: dict[str, float] = {}
    notes: list[str] = []
    groups = sorted(rules.scoring.items(), key=lambda kv: "goal" in kv[0].lower())  # goalies last
    for group, cats in groups:
        is_goalie = "goal" in group.lower()
        for abbr, pts in cats.items():
            key = stat_key(abbr)
            if key is None:
                notes.append(f"scoring category {rules.scoring_names.get(abbr, abbr)} = {pts:g} is not tracked")
                continue
            if pts == 0:
                continue
            if not is_goalie or key not in base:
                base.setdefault(key, pts)
            elif base[key] != pts:
                goalie[key] = pts
    return base, goalie, notes


# -- scoring weights: check and least-squares fit ------------------------------

FIT_EXCLUDE = frozenset({"GP", FPTS_KEY, "GAA", "SVPCT"})
# Aggregates that are sums of other columns: fitted last so they drop out when collinear.
FIT_LAST = ("PTS", "PPP", "SHP", "SA")


def fit_lines(p: Player) -> list[StatLine]:
    """Fantrax lines usable for scoring checks: GP > 0 and Fantrax FPts present."""
    return [ln for split in ("season", "projected", "prior")
            if (ln := p.lines.get(split)) is not None and ln.gp > 0 and FPTS_KEY in ln.stats]


def weights_check(weights: Mapping[str, float], players: Iterable[Player],
                  goalie_weights: Mapping[str, float] | None = None) -> tuple[float, int]:
    """(mean absolute error in FP/G between sum(weight*stat) and Fantrax's FP/G, players compared).

    Uses each player's first Fantrax line (season, else projected, else prior) that has
    GP > 0, Fantrax FPts and at least one weighted stat. NaN when no player qualifies."""
    scoring = PointsScoring({k: v for k, v in weights.items() if k != FPTS_KEY}, dict(goalie_weights or {}))
    errs: list[float] = []
    for p in players:
        for line in fit_lines(p):
            if not any(k in line.stats for k in scoring.weights):
                continue
            errs.append(abs(scoring.value(line.per_game()) - line.stats[FPTS_KEY] / line.gp))
            break
    return (sum(errs) / len(errs) if errs else float("nan")), len(errs)


def validate_weights(weights: Mapping[str, float], players: Iterable[Player]) -> float:
    """Mean absolute error (FP/G) of FANTRAX_POINTS against Fantrax's own FPts; NaN if nothing to compare."""
    return weights_check(weights, players)[0]


def _itemized(p: Player, weights: Mapping[str, float]) -> bool:
    return any(k in line.stats for line in fit_lines(p) for k in weights if k != FPTS_KEY)


@dataclass
class FitResult:
    weights: dict[str, float]                 # merged (skater weights + goalie-only stats)
    mae: float                                # mean |error| in FP/G of the rounded weights
    n: int                                    # lines fitted
    residual_max: float                       # worst |error| in FP/G
    goalie_weights: dict[str, float] = field(default_factory=dict)  # goalie values differing from skaters
    groups: dict[str, dict[str, float]] = field(default_factory=dict)

    @property
    def points_line(self) -> str:
        return "FANTRAX_POINTS=" + format_points(self.weights, self.goalie_weights)


def _lstsq(rows: list[list[float]], y: list[float], ncols: int) -> list[float]:
    """Least squares (numpy if installed, else pure Python). Columns that are all zero or a
    linear combination of earlier columns are dropped first (Gram-Schmidt) and get weight 0."""
    cols = [[r[j] for r in rows] for j in range(ncols)]
    basis: list[list[float]] = []
    keep: list[int] = []
    for j, c in enumerate(cols):
        v = list(c)
        for b in basis:
            d = sum(x * z for x, z in zip(v, b))
            v = [x - d * z for x, z in zip(v, b)]
        norm = math.sqrt(sum(x * x for x in v))
        scale = math.sqrt(sum(x * x for x in c))
        if norm > 1e-12 and norm > 1e-9 * scale:
            basis.append([x / norm for x in v])
            keep.append(j)
    out = [0.0] * ncols
    if not keep:
        return out
    try:
        import numpy as np  # type: ignore[import-not-found]
    except ImportError:
        np = None
    if np is not None:
        X = np.array([[r[j] for j in keep] for r in rows], dtype=float)
        sol = np.linalg.lstsq(X, np.array(y, dtype=float), rcond=None)[0]
        for j, w in zip(keep, sol):
            out[j] = float(w)
        return out
    k, n = len(keep), len(rows)
    A = [[sum(cols[a][i] * cols[b][i] for i in range(n)) for b in keep] for a in keep]
    rhs = [sum(cols[a][i] * y[i] for i in range(n)) for a in keep]
    for i in range(k):  # Gaussian elimination with partial pivoting
        piv = max(range(i, k), key=lambda r: abs(A[r][i]))
        A[i], A[piv] = A[piv], A[i]
        rhs[i], rhs[piv] = rhs[piv], rhs[i]
        if abs(A[i][i]) < 1e-12:
            continue
        for r in range(i + 1, k):
            f = A[r][i] / A[i][i]
            if f:
                A[r] = [a - f * b for a, b in zip(A[r], A[i])]
                rhs[r] -= f * rhs[i]
    sol = [0.0] * k
    for i in range(k - 1, -1, -1):
        if abs(A[i][i]) < 1e-12:
            continue
        sol[i] = (rhs[i] - sum(A[i][c] * sol[c] for c in range(i + 1, k))) / A[i][i]
    for j, w in zip(keep, sol):
        out[j] = w
    return out


def _fit_group(lines: list[StatLine]) -> dict[str, float]:
    present = {k for ln in lines for k, v in ln.stats.items() if k not in FIT_EXCLUDE and v}
    order = [k for k in CANONICAL_STATS_ORDER if k in present and k not in FIT_LAST]
    order += sorted(present - set(order) - set(FIT_LAST)) + [k for k in FIT_LAST if k in present]
    rows = [[float(ln.stats.get(k, 0.0)) for k in order] for ln in lines]
    y = [float(ln.stats[FPTS_KEY]) for ln in lines]
    out: dict[str, float] = {}
    for k, v in zip(order, _lstsq(rows, y, len(order))):
        v = round(v, 2)
        if abs(v) >= 0.05:
            out[k] = v
    return out


def fit_weights(players: Iterable[Player]) -> FitResult:
    """Least-squares fit of Fantrax FPts totals on per-stat totals.

    Skaters and goalies are fitted separately (one row per Fantrax line with GP > 0 and FPts),
    weights are rounded to 2 decimals and |w| < 0.05 is dropped. The skater fit is the base;
    goalie-only stats are added to it and goalie values that differ from the skater value of
    the same stat are returned as `goalie_weights` (FANTRAX_POINTS `goalie.` entries)."""
    groups: dict[str, list[StatLine]] = {"skaters": [], "goalies": []}
    for p in players:
        for ln in fit_lines(p):
            if any(k not in FIT_EXCLUDE and v for k, v in ln.stats.items()):
                groups["goalies" if p.is_goalie else "skaters"].append(ln)
    fitted = {g: _fit_group(lines) if lines else {} for g, lines in groups.items()}
    base = dict(fitted["skaters"])
    goalie: dict[str, float] = {}
    for k, v in fitted["goalies"].items():
        if k not in base:
            base[k] = v
        elif base[k] != v:
            goalie[k] = v
    scoring = PointsScoring(base, goalie)
    errs = [abs(scoring.value(ln.per_game()) - ln.stats[FPTS_KEY] / ln.gp)
            for lines in groups.values() for ln in lines]
    return FitResult(weights=base, mae=(sum(errs) / len(errs)) if errs else float("nan"), n=len(errs),
                     residual_max=max(errs) if errs else float("nan"), goalie_weights=goalie, groups=fitted)


def season_from_settings(fs: Mapping[str, Any], today: date | None = None) -> int:
    """Season labelled by the year it ends (2026-27 -> 2027), like the ESPN provider."""
    end = (fs.get("season") or {}).get("endDate")
    if isinstance(end, (int, float)) and end > 0:
        return datetime.fromtimestamp(end / 1e3).year
    years = [int(y) for y in re.findall(r"(20\d\d)", str(fs.get("subtitle") or ""))]
    if years:
        y = max(years)
        m = re.search(r"20\d\d\s*[-/]\s*(\d\d)\b", str(fs.get("subtitle") or ""))
        return 2000 + int(m.group(1)) if m else y
    return default_espn_year(today)


def find_rule_settings(obj: Any, path: str = "", out: dict[str, Any] | None = None) -> dict[str, Any]:
    """Collect scalar settings whose key path mentions roster/dynasty keywords."""
    out = {} if out is None else out
    if len(out) >= MAX_RULE_LINES * 3:
        return out
    if isinstance(obj, Mapping):
        for k, v in obj.items():
            p = f"{path}.{k}" if path else str(k)
            if isinstance(v, (Mapping, list)):
                find_rule_settings(v, p, out)
            elif RULE_KEYWORDS.search(str(k)) and v not in (None, "", [], {}):
                if not (isinstance(v, str) and len(v) > 200):
                    out[p] = v
    elif isinstance(obj, list):
        for i, v in enumerate(obj[:50]):
            find_rule_settings(v, f"{path}[{i}]", out)
    return out


# -- provider ---------------------------------------------------------------

class FantraxProvider:
    """Loads a Fantrax league into a LeagueContext.

    After `load()`: `fantasy_points` ({cid: (FPts, FP/G)} - season to date once a player has
    games, else Fantrax's full-season projection), `ages` ({cid: age from Fantrax}), `rules`
    (the league's Rules page), `draft_picks`, `standings`, `trade_blocks`, `pending_trades`,
    `transactions` and `warnings` are populated.
    """

    def __init__(self, settings: Settings, cache: HttpCache | None = None, session: Any = None,
                 fetch_free_agents: bool = True, today: date | None = None, auth: Any = None):
        self.settings = settings
        self.cache = cache
        self._session = session
        self._auth = auth
        self._relogged = False
        self.fetch_free_agents = fetch_free_agents
        self.today = today
        self.warnings: list[str] = []
        self.fantasy_points: dict[str, tuple[float | None, float | None]] = {}
        self.ages: dict[str, float] = {}
        self.standings: dict[str, dict[str, Any]] = {}
        self.trade_blocks: list[TradeBlockInfo] = []
        self.pending_trades: list[PendingTradeInfo] = []
        self.transactions: list[TransactionInfo] = []
        self.weights_mae: float = float("nan")
        self.weights_checked: int = 0
        self.scoring_source: str = ""
        self._raw_info: dict[str, Any] = {}
        self._limits: dict[str, tuple[int, int]] = {}
        self._ctx: LeagueContext | None = None
        self._fa_ids: set[str] = set()
        self._client: FxpaClient | None = None
        self.rules: LeagueRules | None = None          # parsed Rules page (getLeagueRulesOld)
        self.draft_picks: dict[int, list[dict[str, Any]]] = {}  # my picks by year
        self.config_mae: float = float("nan")          # FANTRAX_POINTS vs Fantrax FPts
        self.config_checked: int = 0
        self._codes: dict[str, dict[str, str]] = {}    # split -> timeframe request params
        self._misc: dict[str, Any] = {}
        self._teams_meta: dict[str, dict[str, str]] = {}
        self._my_id: str | None = None

    # -- plumbing ----------------------------------------------------------
    @property
    def offline(self) -> bool:
        return bool(self.settings.fm_offline or getattr(self.cache, "offline", False))

    def _get_client(self) -> FxpaClient:
        if self._client is not None:
            return self._client
        s = self.settings
        if not s.fantrax_league_id:
            raise ProviderError("FANTRAX_LEAGUE_ID is not set. It is the id in your league URL: "
                                "fantrax.com/fantasy/league/<FANTRAX_LEAGUE_ID>/home.")
        session = self._session
        if session is None:
            if self.offline:
                try:
                    session = self.auth.ensure_session(allow_login=False)
                except ProviderError:
                    session = build_session({})
            else:
                session = self.auth.ensure_session()
        cache_dir = Path(s.fm_data_dir) / "fantrax_cache"
        self._client = FxpaClient(str(s.fantrax_league_id), session, cache_dir, offline=self.offline,
                                  relogin=None if self.offline else self._relogin)
        return self._client

    @property
    def auth(self) -> Any:
        """The FantraxAuth that finds / refreshes the cookie session."""
        if self._auth is None:
            from .fantrax_auth import FantraxAuth

            self._auth = FantraxAuth(self.settings)
        return self._auth

    def _relogin(self) -> Any:
        """FxpaClient hook for WARNING_NOT_LOGGED_IN: at most one recovery per provider."""
        if self._relogged:
            return None
        self._relogged = True
        session = self.auth.recover()
        if session is not None and self.auth.source == "login":
            msg = "Fantrax session refreshed by login"
            if msg not in self.warnings:
                self.warnings.append(msg)
        return session

    def ping(self) -> dict[str, Any]:
        """One uncached authenticated request (getFantasyLeagueInfo) to check / keep the session alive."""
        try:
            info = self._call(("getFantasyLeagueInfo", {}), fresh=True)[0]
        except ProviderError as e:
            self.auth.record_ping(False, type(e).__name__)
            raise
        self.auth.record_ping(True)
        client = self._get_client()
        settings_ = info.get("fantasySettings") if isinstance(info, dict) else None
        name = settings_.get("leagueName") if isinstance(settings_, dict) else None
        return {"ok": True, "league_id": client.league_id, "league_name": name,
                "session_source": self.auth.source,
                "refreshed": self._relogged and self.auth.source == "login",
                "requests_made": client.requests_made}

    def _call(self, *calls: tuple[str, Mapping[str, Any]], fresh: bool = False) -> list[dict]:
        """Calls that must succeed: Fantrax errors become ProviderError with advice."""
        import requests
        from fantraxapi.exceptions import FantraxException, NotLoggedIn, NotMemberOfLeague

        client = self._get_client()
        lid = client.league_id
        try:
            return client.call(*calls, fresh=fresh)
        except NotLoggedIn as e:
            if self._relogged and self.auth.source == "login":
                raise ProviderError(
                    "Fantrax accepted the login (FANTRAX_USERNAME / FANTRAX_PASSWORD) but still says you are "
                    "not logged in. Use a browser cookie instead: log in at fantrax.com, open DevTools > "
                    "Network, click any 'req?leagueId=' request and copy the whole Cookie request header "
                    "into FANTRAX_COOKIE. See 'Fantrax setup' in the README.") from e
            raise ProviderError(
                "Fantrax says you are not logged in: the saved session or FANTRAX_COOKIE is missing, expired "
                "or incomplete. Recommended: set FANTRAX_USERNAME and FANTRAX_PASSWORD in .env so fm logs in "
                "itself and refreshes the session. Or log in at fantrax.com, open DevTools > Network, click "
                "any 'req?leagueId=' request and copy the whole Cookie request header into FANTRAX_COOKIE (or "
                "save it to the file named by FANTRAX_COOKIE_FILE). See 'Fantrax setup' in the README.") from e
        except NotMemberOfLeague as e:
            raise ProviderError(f"The logged-in Fantrax account is not a member of league {lid}. "
                                "Check FANTRAX_LEAGUE_ID and that the cookie belongs to your account.") from e
        except CacheMiss as e:
            raise ProviderError(f"Offline mode and Fantrax data not cached: {e}") from e
        except FantraxException as e:
            raise ProviderError(f"Fantrax request failed for league {lid}: {e}. If this persists, "
                                "run `fm auth fantrax --login` (or refresh FANTRAX_COOKIE) and check "
                                "FANTRAX_LEAGUE_ID.") from e
        except requests.RequestException as e:
            raise ProviderError(f"Could not reach Fantrax: {type(e).__name__}") from e

    def _try(self, label: str, *calls: tuple[str, Mapping[str, Any]]) -> list[dict] | None:
        """Best-effort calls: failures are recorded in `warnings`."""
        try:
            return self._call(*calls)
        except ProviderError as e:
            self.warnings.append(f"{label} unavailable: {e}")
        except Exception as e:  # noqa: BLE001 - unexpected shapes must not break load()
            self.warnings.append(f"{label} unavailable: {type(e).__name__}: {e}")
        return None

    # -- public API --------------------------------------------------------
    def load(self) -> LeagueContext:
        if self._ctx is not None:
            return self._ctx
        s = self.settings
        info, my_roster_raw = self._call(("getFantasyLeagueInfo", {}),
                                         ("getTeamRosterInfo", {"view": "STATS"}))
        self._raw_info = info
        self._codes = timeframe_codes(my_roster_raw)
        self.draft_picks = parse_draft_picks(my_roster_raw)
        self._misc = dict(my_roster_raw.get("miscData") or {})
        fs = info.get("fantasySettings") or {}
        positions = parse_positions(info.get("positionMap"))
        teams_meta = parse_teams(my_roster_raw) or parse_teams(info)
        if not teams_meta:
            raise ProviderError("Fantrax returned no teams for this league. Check FANTRAX_LEAGUE_ID.")
        my_id = self._my_team_id(teams_meta, my_roster_raw.get("myTeamIds") or [])
        self._teams_meta = teams_meta
        self._my_id = my_id

        team_ids = list(teams_meta)
        parsed_rosters = self._rosters(team_ids, positions)

        standings_raw = self._try("Standings", ("getStandings", {}))
        if standings_raw:
            try:
                self.standings = parse_standings(standings_raw[0])
            except Exception as e:  # noqa: BLE001
                self.warnings.append(f"Standings could not be parsed: {e}")

        teams: list[FantasyTeam] = []
        slot_counts: dict[str, int] = {}
        for tid, parsed in zip(team_ids, parsed_rosters):
            self.fantasy_points.update(parsed.fantasy_points)
            self.ages.update(parsed.ages)
            if tid == my_id:
                slot_counts, self._limits = parsed.slot_counts, parsed.limits
            st = self.standings.get(tid)
            teams.append(FantasyTeam(
                team_id=tid, name=teams_meta[tid]["name"], owner_is_me=(tid == my_id), slots=parsed.slots,
                record=(st["win"], st["loss"], st["tie"]) if st else None))

        rostered = {p.cid for t in teams for p in t.players}
        if self.fetch_free_agents:
            self._rostered_ownership(teams, positions)
        free_agents = self._free_agents(rostered, positions) if self.fetch_free_agents else []
        self._fa_ids = {p.cid for p in free_agents}
        if not self.fetch_free_agents:
            self.warnings.append("Free-agent pool disabled; replacement level uses rostered players.")

        self._load_extras(positions, teams_meta)
        self._load_rules()
        all_players = [p for t in teams for p in t.players] + free_agents
        scoring = self._scoring(all_players)

        self._ctx = LeagueContext(
            provider="fantrax",
            league_id=str(s.fantrax_league_id),
            season=season_from_settings(fs, self.today),
            name=str(fs.get("leagueName") or f"Fantrax {s.fantrax_league_id}"),
            scoring=scoring,
            roster_shape=self._roster_shape(slot_counts),
            teams=teams,
            free_agents=free_agents,
            matchup_period=None,
            dynasty=bool(s.fantrax_dynasty) or self.rules_dynasty,
            keeper_horizon_years=int(s.fantrax_keeper_horizon_years),
            dynasty_mode=getattr(s, "fantrax_mode", None) or "balanced",
            as_of=self.today or date.today(),
        )
        return self._ctx

    def _rosters(self, team_ids: list[str], positions: Mapping[str, str]) -> list[ParsedRoster]:
        """Every team's roster, with season-to-date and full-season projection lines.

        The default roster view is a per-period projection, so the season and projection
        timeframes are requested explicitly (one batched request each). Without timeframe
        codes (older responses) the default view is parsed as before."""
        base = {"view": "STATS"}
        views = [(split, self._codes[split]) for split in SPLIT_TIMEFRAMES if split in self._codes]
        if not views:
            raw = self._call(*[("getTeamRosterInfo", {**base, "teamId": tid}) for tid in team_ids])
            return [parse_roster(r, positions) for r in raw]
        per_team: list[list[ParsedRoster]] = [[] for _ in team_ids]
        for i, (split, params) in enumerate(views):
            calls = [("getTeamRosterInfo", {**base, "teamId": tid, **params}) for tid in team_ids]
            raw = self._call(*calls) if i == 0 else self._try(f"Fantrax {split} roster stats", *calls)
            for j, r in enumerate(raw or []):
                per_team[j].append(parse_roster(r, positions, self._codes))
        return [merge_rosters(prs) for prs in per_team]

    def _roster_shape(self, slot_counts: Mapping[str, int]) -> dict[str, int]:
        """Starting slots from my roster, bench/IR/minors from status limits; the Rules page
        (position max-active counts, reserve / IR / minors maximums) wins when available."""
        shape = roster_shape(slot_counts, self._limits)
        r = self.rules
        if r is None or r.empty:
            return shape
        pos_max = {p: v["max_active"] for p, v in r.positions.items() if v.get("max_active")}
        if pos_max:  # the Rules page lists every starting position (incl. flex) with its max
            shape = {k: v for k, v in shape.items() if k in NON_STARTING}
            shape.update({canonical_slot(p): n for p, n in pos_max.items()})
        for label, slot in (("Maximum Reserve Players", "BN"), ("Maximum Injury Reserve Players", "IR"),
                            ("Maximum Minor League Players", "MIN")):
            val = r.get(label)
            if val is None:
                continue
            n = r.get_int(label)
            if n and n > 0:
                shape[slot] = n
            else:  # "Not Used"
                shape.pop(slot, None)
        return shape

    @property
    def rules_dynasty(self) -> bool:
        return bool(self.rules and "dynasty" in (self.rules.get("Keeper league Type") or "").lower())

    def _my_team_id(self, teams: Mapping[str, Mapping[str, str]], my_ids: list[str]) -> str:
        hint = (self.settings.fantrax_team or "").strip()
        if hint:
            if hint in teams:
                return hint
            low = hint.lower()
            for tid, t in teams.items():
                if low == t["short"].lower() or low in t["name"].lower():
                    return tid
        else:
            for tid in my_ids:
                if str(tid) in teams:
                    return str(tid)
        listing = ", ".join(f"{tid}={t['name']}" for tid, t in teams.items())
        what = f"FANTRAX_TEAM={hint!r} matches no team" if hint else "Could not identify your Fantrax team"
        raise ProviderError(f"{what}. Set FANTRAX_TEAM to one of: {listing}")

    def _load_rules(self) -> None:
        r = self._try("League rules", ("getLeagueRulesOld", {}))
        if not r:
            return
        try:
            self.rules = parse_league_rules(str(r[0].get("content") or ""))
        except Exception as e:  # noqa: BLE001
            self.warnings.append(f"League rules could not be parsed: {e}")

    def _scoring(self, players: list[Player]) -> ScoringConfig:
        """Point values: the league's Rules page, else FANTRAX_POINTS, else Fantrax's own FP/G."""
        cfg_base, cfg_goalie = split_points(dict(self.settings.fantrax_points or {}))
        if cfg_base:
            self.config_mae, self.config_checked = weights_check(cfg_base, players, cfg_goalie)
        has_fpts = any(fit_lines(p) for p in players)
        weights, goalie = {}, {}
        if self.rules is not None and self.rules.scoring:
            weights, goalie, notes = rules_weights(self.rules)
            self.warnings.extend(f"Fantrax {n}" for n in notes)
            self.scoring_source = "Fantrax league rules"
        if not weights and cfg_base:
            weights, goalie = cfg_base, cfg_goalie
            self.scoring_source = "FANTRAX_POINTS"
        if not weights:
            self.scoring_source = "Fantrax FP/G (FANTRAX_POINTS not set)"
            self.warnings.append("FANTRAX_POINTS is not set: using Fantrax's own FP/G. Prior-season and "
                                 "projected stats cannot be valued; set FANTRAX_POINTS for full valuations.")
            return ScoringConfig(kind="points", weights={FPTS_KEY: 1.0})
        self.weights_mae, self.weights_checked = weights_check(weights, players, goalie)
        fas = [p for p in players if p.cid in self._fa_ids and fit_lines(p)]
        if fas and not any(_itemized(p, weights) for p in fas):
            self.warnings.append("The Fantrax free-agent list has no per-stat columns, so free agents are "
                                 "valued from other stat sources only (Fantrax FP/G is in "
                                 "FantraxProvider.fantasy_points).")
        if math.isnan(self.weights_mae) and has_fpts:
            self.warnings.append(f"Fantrax showed no per-stat columns, so the {self.scoring_source} point "
                                 "values could not be checked against Fantrax FPts.")
        elif not math.isnan(self.weights_mae) and self.weights_mae > 0.25:
            self.warnings.append(f"{self.scoring_source} point values differ from Fantrax's FP/G by "
                                 f"{self.weights_mae:.2f} per game on average; run "
                                 "'fm --league fantrax settings --fit-points'.")
        return ScoringConfig(kind="points", weights=weights, goalie_weights=goalie)

    def _load_extras(self, positions: Mapping[str, str], teams: Mapping[str, Mapping[str, str]]) -> None:
        if (r := self._try("Trade blocks", ("getTradeBlocks", {}))) is not None:
            try:
                self.trade_blocks = parse_trade_blocks(r[0].get("tradeBlocks") or [], positions, teams)
            except Exception as e:  # noqa: BLE001
                self.warnings.append(f"Trade blocks could not be parsed: {e}")
        if (r := self._try("Pending trades", ("getPendingTransactions", {}))) is not None:
            try:
                self.pending_trades = parse_pending_trades(r[0])
            except Exception as e:  # noqa: BLE001
                self.warnings.append(f"Pending trades could not be parsed: {e}")
        if (r := self._try("Transactions", ("getTransactionDetailsHistory", {"maxResultsPerPage": "50"}))) is not None:
            try:
                self.transactions = parse_transactions(r[0])
            except Exception as e:  # noqa: BLE001
                self.warnings.append(f"Transactions could not be parsed: {e}")

    # -- harness capture (best effort: failures become warnings) ----------------------
    def _warn(self, msg: str) -> None:
        self.warnings.append(msg)
        if self._ctx is not None:
            self._ctx.warnings.append(msg)

    def activity(self, since: date | None = None) -> list[ActivityItem]:
        """Recent transactions (last 50: claims, drops, trades) plus pending trade proposals
        as ActivityItems, filtered to ``since``. Loads the league first when needed."""
        try:
            ctx = self.load()
        except Exception as e:  # noqa: BLE001
            self._warn(f"Fantrax activity unavailable: {e}")
            return []
        names = {t.team_id: t.name for t in ctx.teams}
        y0 = season_start_year_for(ctx.as_of)
        try:
            items = activity_from_transactions(self.transactions, names, y0)
            items += activity_from_pending(self.pending_trades, names, y0,
                                           now=datetime.combine(ctx.as_of, datetime.min.time()))
        except Exception as e:  # noqa: BLE001
            self._warn(f"Fantrax activity could not be parsed: {type(e).__name__}: {e}")
            return []
        return [i for i in items if since is None or i.ts.date() >= since]

    def lineup_snapshot(self, day: date | None = None) -> list[LineupDay]:
        """Every team's current lineup from the loaded rosters, labelled ``day`` (default the
        context's as_of). Fantrax lineups are weekly, so a snapshot taken any day after the
        Monday lock is that scoring period's lineup."""
        try:
            ctx = self.load()
            return lineup_days_from_teams(ctx.teams, day or ctx.as_of)
        except Exception as e:  # noqa: BLE001
            self._warn(f"Fantrax lineup snapshot unavailable: {e}")
            return []

    def pool_queries(self, positions: Mapping[str, str]) -> list[tuple[str, dict[str, str], int]]:
        """(label, getPlayerStats params, max pages) for the free-agent pool.

        Fantrax's default group is skaters only ("HOCKEY_SKATING"; the "ALL" group has no stat
        columns), so goalies need their own positionOrGroup query. Each group is fetched for
        the full-season projection and the season-to-date timeframes."""
        gid = next((pid for pid, short in positions.items() if canonical_slot(short) == "G"), None)
        groups = [("skaters", SKATER_GROUP, FA_MAX_PAGES),
                  ("goalies", f"POS_{gid}" if gid else GOALIE_GROUP_DEFAULT, FA_GOALIE_PAGES)]
        timeframes = [(split, self._codes[split]) for split in ("projected", "season") if split in self._codes]
        out = []
        for split, tf in timeframes or [("default", {})]:
            for label, group, pages in groups:
                out.append((f"{label} {split}", {"statusOrTeamFilter": "ALL_AVAILABLE", "view": "STATS",
                                                 "positionOrGroup": group, **tf}, pages))
        return out

    def _player_pool(self, positions: Mapping[str, str]) -> tuple[list[Player], dict[str, dict[str, RowStats]]]:
        """Available players via raw getPlayerStats: {cid: player} merged across queries, and
        {cid: {split: row stats}}. A query stops paging once a page has no player with a line."""
        client = self._get_client()
        players: dict[str, Player] = {}
        rows: dict[str, dict[str, RowStats]] = {}
        errors: list[str] = []
        last_err: Exception | None = None
        ok = 0
        for label, params, max_pages in self.pool_queries(positions):
            page, pages = 1, 1
            try:
                while page <= min(pages, max_pages):
                    data = client.call(("getPlayerStats", {**params, "maxResultsPerPage": str(FA_PAGE_SIZE),
                                                           "pageNumber": str(page)}))[0]
                    split = data_split(data, self._codes or None)
                    got, got_rows, pages = parse_player_pool(data, self._codes or None)
                    ok += 1
                    useful = [p for p in got if p.lines or split is None]
                    for p in useful:
                        if p.cid in players:
                            merge_lines(players[p.cid], p)
                        else:
                            players[p.cid] = p
                        rows.setdefault(p.cid, {})[split or "?"] = got_rows[p.cid]
                    if not useful:
                        break
                    page += 1
            except Exception as e:  # noqa: BLE001 - other queries may still work
                last_err = e
                errors.append(f"{label}: {type(e).__name__}: {e}")
        if not ok:
            raise last_err or LookupError("getPlayerStats returned no available players")
        if errors:
            self.warnings.append("Some Fantrax free-agent queries failed: " + "; ".join(errors))
        if not players:
            raise LookupError("getPlayerStats returned no available players")
        return list(players.values()), rows

    def _rostered_ownership(self, teams: list[FantasyTeam], positions: Mapping[str, str]) -> None:
        """Fill `pct_owned` (Fantrax-wide % rostered) and missing ages for rostered players from
        getPlayerStats with statusOrTeamFilter=ALL_TAKEN (skaters and goalies, one page each
        covers a full league). Best effort: failures only add a warning."""
        by_cid = {p.cid: p for t in teams for p in t.players}
        if not by_cid:
            return
        gid = next((pid for pid, short in positions.items() if canonical_slot(short) == "G"), None)
        tf = self._codes.get("projected") or self._codes.get("season") or {}
        found = 0
        for group in (SKATER_GROUP, f"POS_{gid}" if gid else GOALIE_GROUP_DEFAULT):
            params = {"statusOrTeamFilter": "ALL_TAKEN", "view": "STATS", "positionOrGroup": group, **tf}
            page, pages = 1, 1
            while page <= min(pages, TAKEN_MAX_PAGES):
                r = self._try("Fantrax rostered-player ownership",
                              ("getPlayerStats", {**params, "maxResultsPerPage": str(FA_PAGE_SIZE),
                                                  "pageNumber": str(page)}))
                if not r:
                    break
                for cid, (owned, age) in parse_taken_ownership(r[0]).items():
                    p = by_cid.get(cid)
                    if p is None:
                        continue
                    if owned is not None and p.pct_owned is None:
                        p.pct_owned = owned
                        found += 1
                    if age is not None:
                        self.ages.setdefault(cid, age)
                pages = int((r[0].get("paginatedResultSet") or {}).get("totalNumPages") or 1)
                page += 1
        if found == 0:
            self.warnings.append("Fantrax % rostered unavailable for rostered players (market prior uses "
                                 "free agents only).")

    def _free_agents(self, rostered: set[str], positions: Mapping[str, str] | None = None) -> list[Player]:
        try:
            players, rows = self._player_pool(positions or {})
        except Exception as e:  # noqa: BLE001
            reason = "not cached (offline)" if isinstance(e, CacheMiss) else f"{type(e).__name__}: {e}"
            self.warnings.append(f"Fantrax free-agent pool unavailable ({reason}); replacement level "
                                 "falls back to rostered players.")
            return []
        out: dict[str, Player] = {}
        for p in players:
            if p.cid in rostered or p.cid in out:
                continue
            out[p.cid] = p
            by_split = rows.get(p.cid, {})
            fp = pick_fantasy_points({k: (v.fpts, v.fpg) for k, v in by_split.items()}, p)
            if fp is not None:
                self.fantasy_points[p.cid] = fp
            age = next((v.age for v in by_split.values() if v.age is not None), None)
            if age is not None:
                self.ages[p.cid] = age
        return list(out.values())

    def fit_points(self) -> FitResult:
        """Least-squares scoring weights fitted on every loaded player's Fantrax lines."""
        return fit_weights(self.load().all_players())

    def raw_settings(self) -> dict[str, Any]:
        """League settings as Fantrax returned them plus roster/dynasty rules."""
        if self._ctx is None:
            self.load()
        fs = self._raw_info.get("fantasySettings") or {}
        out: dict[str, Any] = {
            "fantasySettings": fs,
            "roster_limits": {k: {"total": t, "max": m} for k, (t, m) in self._limits.items()},
            "rules": find_rule_settings({k: v for k, v in self._raw_info.items() if k != "positionMap"}),
            "draft_picks": self.draft_picks,
            "timeframes": self._codes,
        }
        for k in ("draftPickTradingAllowed", "tradesAllowed", "claimsDropsAllowed"):
            if k in self._misc:
                out[k] = self._misc[k]
        if self.rules is not None and not self.rules.empty:
            out["league_rules"] = {sect: {k: v for k, v in kv.items() if k not in RULE_SKIP_LABELS and v}
                                   for sect, kv in self.rules.settings.items()}
            out["scoring_rules"] = self.rules.scoring
            out["position_limits"] = self.rules.positions
        return out

    def rule_summary(self) -> list[str]:
        """Headline roster / dynasty rules from the Rules page (empty if it was not read)."""
        r = self.rules
        if r is None or r.empty:
            return []
        lines: list[str] = []

        def val(label: str) -> str | None:
            v = r.get(label)
            return v if v not in (None, "") else None

        parts = [f"{lab} {v}" for lab, key in (("total", "Maximum Total Players"),
                                                ("active", "Maximum Active Players"),
                                                ("reserve", "Maximum Reserve Players"),
                                                ("IR", "Maximum Injury Reserve Players"),
                                                ("minors", "Maximum Minor League Players"))
                 if (v := val(key)) is not None]
        if parts:
            lines.append("Roster: max " + ", ".join(parts))
        if r.positions:
            lines.append("Active by position (max): " + ", ".join(
                f"{p} {v['max_active']}" for p, v in r.positions.items() if v.get("max_active") is not None))
        if (v := val("Keeper league Type")):
            lines.append(f"Keeper league type: {v}")
        if (v := val("Allow trading of draft Picks")):
            extra = []
            if (y := val("Number of future years for draft pick trading")):
                extra.append(f"{y} future years")
            if (n := val("Number of rounds available for draft pick trading")):
                extra.append(f"{n} rounds")
            lines.append(f"Draft picks tradeable: {v}" + (f" ({', '.join(extra)})" if extra else ""))
        if (v := val("Lineup changes are executed")):
            lines.append(f"Lineup changes: {v}")
        if (v := val("Playoffs will begin in this Scoring Period")):
            teams = val("Number of teams qualifying for playoffs")
            lines.append(f"Playoffs: start scoring period {v}" + (f", {teams} teams" if teams else ""))
        for label in ("Draft Type", "# of rounds", "Trade Deadline Date", "Trade Voting System",
                      "Waiver Wire claim system", "# of days players remain on waivers for"):
            if (v := val(label)):
                lines.append(f"{label}: {v}")
        return lines

    def describe_settings(self) -> list[str]:
        """Human-readable lines for `fm settings`."""
        ctx = self.load()
        raw = self.raw_settings()
        lines = [f"League: {ctx.name} (Fantrax {ctx.league_id}), season {ctx.season}, {len(ctx.teams)} teams"]
        try:
            lines.append(f"My team: {ctx.my_team.name} ({ctx.my_team.team_id})")
        except LookupError:
            pass
        if self._limits:
            lines.append("Roster limits: " + ", ".join(f"{k} {t}/{m}" for k, (t, m) in self._limits.items()))
        if ctx.roster_shape:
            lines.append("Roster shape: " + ", ".join(f"{k}={v}" for k, v in ctx.roster_shape.items()))
        w = ", ".join(f"{k}={v:g}" for k, v in ctx.scoring.weights.items())
        if ctx.scoring.goalie_weights:
            w += "; goalies: " + ", ".join(f"{k}={v:g}" for k, v in ctx.scoring.goalie_weights.items())
        lines.append(f"Scoring ({self.scoring_source or 'points'}): {w}")
        label = "FANTRAX_POINTS check" if self.scoring_source == "FANTRAX_POINTS" else "Scoring check"
        if not math.isnan(self.weights_mae):
            lines.append(f"{label}: mean abs error {self.weights_mae:.3f} FP/G vs Fantrax "
                         f"FPts over {self.weights_checked} players with per-stat columns")
        if self.scoring_source != "FANTRAX_POINTS" and not math.isnan(self.config_mae):
            lines.append(f"FANTRAX_POINTS (not used: league rules win) check: mean abs error "
                         f"{self.config_mae:.3f} FP/G over {self.config_checked} players")
            if self.config_mae > FIT_HINT_MAE and ctx.scoring.weights:
                lines.append("  To match the league rules set: FANTRAX_POINTS="
                             + format_points(ctx.scoring.weights, ctx.scoring.goalie_weights))
        mae = self.weights_mae if self.scoring_source == "FANTRAX_POINTS" else float("nan")
        if not math.isnan(mae) and mae > FIT_HINT_MAE:
            lines.append("Hint: run 'fm --league fantrax settings --fit-points' to fit point values "
                         "from Fantrax's own FPts and print a ready-to-paste FANTRAX_POINTS line.")
        dyn_src = "Fantrax keeper league type" if self.rules_dynasty else "FANTRAX_DYNASTY"
        lines.append(f"Dynasty: {'yes' if ctx.dynasty else 'no'}"
                     + (f" (from {dyn_src}; keeper horizon {ctx.keeper_horizon_years} years)" if ctx.dynasty else ""))
        summary = self.rule_summary()
        if summary:
            lines.append("League rules (Fantrax Rules page):")
            lines.extend(f"  {x}" for x in summary)
        if self.draft_picks:
            mine = self._my_id
            parts = []
            for year, picks in sorted(self.draft_picks.items()):
                rounds = sorted(int(pk["round"]) for pk in picks if pk.get("round") is not None)
                acquired = [pk for pk in picks if pk.get("original_owner") and pk["original_owner"] != mine]
                s = f"{year}: {len(picks)} picks" + (f" (rounds {rounds[0]}-{rounds[-1]})" if rounds else "")
                if acquired:
                    s += ", acquired: " + ", ".join(
                        f"R{pk['round']} from {self._teams_meta.get(str(pk['original_owner']), {}).get('name', pk['original_owner'])}"
                        for pk in acquired)
                parts.append(s)
            lines.append("My draft picks: " + "; ".join(parts))
        if "draftPickTradingAllowed" in raw and not summary:
            lines.append(f"Draft pick trading allowed: {'yes' if raw['draftPickTradingAllowed'] else 'no'}")
        rules = raw["rules"]
        if not summary:
            if rules:
                lines.append("League rules reported by Fantrax:")
                for k, v in list(rules.items())[:MAX_RULE_LINES]:
                    lines.append(f"  {k}: {v}")
            else:
                lines.append("Fantrax league info exposed no keeper/minors/contract/draft settings; "
                             "set FANTRAX_DYNASTY / FANTRAX_KEEPER_HORIZON_YEARS by hand.")
        if self.trade_blocks:
            lines.append(f"Trade blocks: {len(self.trade_blocks)} teams listed")
        if self.pending_trades:
            lines.append(f"Pending trades: {len(self.pending_trades)}")
        n_g = sum(1 for p in ctx.free_agents if p.is_goalie)
        lines.append(f"Free agents loaded: {len(ctx.free_agents)} ({n_g} goalies)")
        lines.extend(f"Warning: {w}" for w in self.warnings)
        return lines
