"""ESPN fantasy hockey provider built on espn_api 0.46 (all HTTP goes through HttpCache)."""
from __future__ import annotations

import json
from datetime import date, datetime, timedelta
from typing import Any, Iterable, Mapping

from ..cache import CacheMiss, HttpCache, install_espn_cache
from ..config import Settings
from ..models import (CANONICAL_STATS, ActivityItem, FantasyTeam, LeagueContext, LineupDay, Player, RosterSlot,
                      ScoringConfig, StatLine, normalize_name)
from .base import ProviderError

ESPN_TTL = 15 * 60
TRENDING_TTL = 6 * 3600
# League-independent ("leaguedefaults") player pool: ESPN-wide % rostered and its change, no
# cookies needed. {year} is the season (ESPN_YEAR).
TRENDING_URL = ("https://lm-api-reads.fantasy.espn.com/apis/v3/games/fhl/seasons/{year}"
                "/segments/0/leaguedefaults/1")
TRENDING_POOL = 300

# espn_api STATS_MAP abbreviations that differ from our canonical keys.
STAT_ALIASES = {"+/-": "PM", "SV%": "SVPCT"}

# ESPN lineup slot names (espn_api POSITION_MAP) and slot ids -> canonical.
SLOT_NAMES = {"Center": "C", "Left Wing": "LW", "Right Wing": "RW", "Forward": "F",
              "Defense": "D", "Goalie": "G", "Util": "UTIL", "Bench": "BN", "IR": "IR"}
SLOT_IDS = {0: "C", 1: "LW", 2: "RW", 3: "F", 4: "D", 5: "G", 6: "UTIL", 7: "BN", 8: "IR"}
FA_POSITIONS = ("Center", "Left Wing", "Right Wing", "Defense", "Goalie")

INJURY_STATUS = {"ACTIVE": "healthy", "NORMAL": "healthy", "DAY_TO_DAY": "dtd", "OUT": "out",
                 "INJURY_RESERVE": "ir", "SUSPENSION": "suspended"}

PRO_TEAMS = {
    "boston bruins": "BOS", "buffalo sabres": "BUF", "calgary flames": "CGY",
    "chicago blackhawks": "CHI", "detroit red wings": "DET", "edmonton oilers": "EDM",
    "carolina hurricanes": "CAR", "los angeles kings": "LAK", "dallas stars": "DAL",
    "new jersey devils": "NJD", "new york islanders": "NYI", "new york rangers": "NYR",
    "ottawa senators": "OTT", "philadelphia flyers": "PHI", "pittsburgh penguins": "PIT",
    "colorado avalanche": "COL", "san jose sharks": "SJS", "st. louis blues": "STL",
    "st louis blues": "STL", "tampa bay lightning": "TBL", "toronto maple leafs": "TOR",
    "vancouver canucks": "VAN", "washington capitals": "WSH", "arizona coyotes": "ARI",
    "anaheim ducks": "ANA", "florida panthers": "FLA", "nashville predators": "NSH",
    "winnipeg jets": "WPG", "columbus blue jackets": "CBJ", "minnesota wild": "MIN",
    "vegas golden knights": "VGK", "seattle kraken": "SEA", "utah hockey club": "UTA",
    "utah mammoth": "UTA",
}


# -- pure parsing helpers (unit-tested without network) ---------------------

def pro_team_abbrev(full_name: str | None) -> str | None:
    """ESPN full team name -> NHL 3-letter code (None for free agents/unknown)."""
    if not full_name or full_name == "Unknown Team":
        return None
    key = full_name.strip().lower()
    if key in PRO_TEAMS:
        return PRO_TEAMS[key]
    if "canadiens" in key:  # constant spells it with an accent (or mojibake)
        return "MTL"
    if key.startswith("utah"):
        return "UTA"
    return None


def canonical_stats(total: dict[str, Any] | None) -> dict[str, float]:
    """Map an espn_api stats 'total' dict to canonical keys, dropping placeholders."""
    out: dict[str, float] = {}
    for k, v in (total or {}).items():
        key = STAT_ALIASES.get(k, k)
        if key in CANONICAL_STATS and v is not None:
            try:
                out[key] = float(v)
            except (TypeError, ValueError):
                continue
    if "PTS" not in out and ("G" in out or "A" in out):
        out["PTS"] = out.get("G", 0.0) + out.get("A", 0.0)
    return out


def _gp(stats: dict[str, float]) -> int:
    for k in ("GP", "GS"):
        if stats.get(k):
            return int(round(stats[k]))
    return 0


def parse_stat_lines(stats: dict[str, Any], year: int) -> dict[str, StatLine]:
    """Convert espn_api Player.stats ({'Total 2027': {'total': {...}}, ...}) to StatLines."""
    wanted = {
        f"Total {year}": "season",
        f"Total {year - 1}": "prior",
        f"Last 7 {year}": "last7",
        f"Last 15 {year}": "last15",
        f"Last 30 {year}": "last30",
        f"Projected {year}": "projected",
    }
    lines: dict[str, StatLine] = {}
    for key, split in wanted.items():
        entry = stats.get(key)
        if not entry or not entry.get("total"):
            continue
        cs = canonical_stats(entry["total"])
        lines[split] = StatLine(split=split, gp=_gp(cs), stats=cs)
    return lines


def map_injury_status(status: Any, injured: bool = False) -> str:
    if isinstance(status, str) and status:
        return INJURY_STATUS.get(status.upper(), "unknown")
    return "unknown" if injured else "healthy"


def positions_from_espn(eligible_slots: Iterable[str], default_position: str | None) -> list[str]:
    order = ["C", "LW", "RW", "F", "D", "G"]
    found = {SLOT_NAMES.get(s) for s in eligible_slots or []}
    pos = [p for p in order if p in found]
    if not pos and default_position:
        mapped = SLOT_NAMES.get(default_position)
        if mapped in order:
            pos = [mapped]
    return pos


def player_from_espn(p: Any, year: int, pct_owned: float | None = None,
                     ownership: Mapping[str, float | None] | None = None) -> Player:
    """Build a Player from an espn_api hockey Player (duck-typed). ``ownership`` (from
    ``ownership_fields``) fills the market fields; its pct_owned is used when ``pct_owned`` is None."""
    own = dict(ownership or {})
    if pct_owned is None:
        pct_owned = own.get("pct_owned")
    status_raw = getattr(p, "injuryStatus", None)
    note = status_raw if isinstance(status_raw, str) and status_raw not in ("", "ACTIVE", "NORMAL") else None
    return Player(
        cid=f"espn:{p.playerId}",
        name=p.name,
        name_norm=normalize_name(p.name),
        ids={"espn": str(p.playerId)},
        team=pro_team_abbrev(getattr(p, "proTeam", None)),
        positions=positions_from_espn(getattr(p, "eligibleSlots", []), getattr(p, "position", None)),
        status=map_injury_status(status_raw, bool(getattr(p, "injured", False))),
        status_note=note,
        lines=parse_stat_lines(getattr(p, "stats", {}) or {}, year),
        pct_owned=pct_owned,
        pct_owned_change=own.get("pct_owned_change"),
        pct_started=own.get("pct_started"),
        adp=own.get("adp"),
        adp_change=own.get("adp_change"),
    )


def _item_key(item: dict) -> str | None:
    from espn_api.hockey.constant import STATS_MAP

    abbrev = STATS_MAP.get(str(item.get("statId")), "")
    key = STAT_ALIASES.get(abbrev, abbrev)
    return key if key in CANONICAL_STATS else None


def scoring_from_settings(scoring_type: str | None, scoring_items: list[dict]) -> ScoringConfig:
    """Build ScoringConfig from ESPN scoringType + scoringItems."""
    st = (scoring_type or "").upper()
    if "CATEGOR" in st:
        kind = "categories"
    elif "ROTO" in st:
        kind = "roto"
    else:
        kind = "points"
    weights: dict[str, float] = {}
    cats: list[str] = []
    for item in scoring_items or []:
        key = _item_key(item)
        if key is None:
            continue
        if kind == "points":
            weights[key] = weights.get(key, 0.0) + float(item.get("points") or 0.0)
        elif key not in cats:
            cats.append(key)
            # categories: +1 higher-is-better, -1 for reverse items (GAA, L, GA...)
            weights[key] = -1.0 if item.get("isReverseItem") else 1.0
    return ScoringConfig(kind=kind, weights=weights, categories=cats)


def roster_shape_from_counts(counts: dict[str, Any]) -> dict[str, int]:
    shape: dict[str, int] = {}
    for sid, n in (counts or {}).items():
        slot = SLOT_IDS.get(int(sid))
        if slot and int(n) > 0:
            shape[slot] = int(n)
    return shape


def _norm_swid(s: Any) -> str:
    return str(s or "").strip().strip("{}").lower()


def find_my_team(teams: list[Any], swid: str | None, team_hint: str | None) -> Any:
    """Return my espn_api Team: ESPN_TEAM (id or name substring) first, then SWID owner match."""
    if team_hint:
        hint = team_hint.strip()
        for t in teams:
            if hint.isdigit() and int(hint) == t.team_id:
                return t
        for t in teams:
            if hint.lower() in str(t.team_name).lower() or hint.lower() == str(t.team_abbrev).lower():
                return t
    if swid:
        target = _norm_swid(swid)
        for t in teams:
            for o in getattr(t, "owners", []) or []:
                if isinstance(o, dict) and _norm_swid(o.get("id")) == target:
                    return t
    listing = ", ".join(f"{t.team_id}={t.team_name}" for t in teams)
    raise ProviderError(f"Could not identify your ESPN team. Set ESPN_TEAM to one of: {listing}")


def _float(val: Any) -> float | None:
    try:
        return float(val) if val is not None else None
    except (TypeError, ValueError):
        return None


def ownership_fields(raw_player: Mapping[str, Any] | None) -> dict[str, float | None]:
    """ESPN ``player.ownership`` block -> Player market fields.

    percentOwned -> pct_owned, percentChange -> pct_owned_change (ESPN's weekly "+/-" of %
    rostered, in points), percentStarted -> pct_started, averageDraftPosition -> adp (None when
    0 / absent), averageDraftPositionPercentChange -> adp_change (free-agent / pool rows only;
    rostered players' league entries omit it)."""
    o = (raw_player or {}).get("ownership") or {}
    adp = _float(o.get("averageDraftPosition"))
    return {"pct_owned": _float(o.get("percentOwned")),
            "pct_owned_change": _float(o.get("percentChange")),
            "pct_started": _float(o.get("percentStarted")),
            "adp": adp if adp and adp > 0 else None,
            "adp_change": _float(o.get("averageDraftPositionPercentChange"))}


def _pct_owned(raw_player: dict) -> float | None:
    return ownership_fields(raw_player)["pct_owned"]


def _ownership_from_league(raw: dict) -> dict[int, dict[str, float | None]]:
    """Raw league JSON (teams with rosters) -> {player id: ownership_fields} per rostered player."""
    out: dict[int, dict[str, float | None]] = {}
    for team in raw.get("teams", []):
        for entry in team.get("roster", {}).get("entries", []):
            pl = entry.get("playerPoolEntry", {}).get("player", {})
            own = ownership_fields(pl)
            if pl.get("id") is not None and any(v is not None for v in own.values()):
                out[pl["id"]] = own
    return out


def trending_filter(year: int, limit: int = TRENDING_POOL, fallers: bool = False) -> dict[str, Any]:
    """X-Fantasy-Filter for the leaguedefaults pool sorted by % rostered change (risers first,
    or fallers first with ``fallers``). Stats are limited to the season total (~0.6 MB instead
    of ~9 MB for 300 players)."""
    return {"players": {"sortPercChanged": {"sortPriority": 1, "sortAsc": bool(fallers)},
                        "limit": int(limit),
                        "filterStatsForTopScoringPeriodIds": {"value": 1, "additionalValue": [f"00{year}"]}}}


def parse_trending(data: Mapping[str, Any] | list, year: int, limit: int = 50,
                   fallers: bool = False) -> list[dict[str, Any]]:
    """leaguedefaults ``kona_player_info`` JSON -> player-lite dicts, largest % rostered rise
    first (only rises > 0), or largest fall first with ``fallers`` (only falls < 0).

    Each dict: cid, espn_id, name, team, positions, status, pct_owned, pct_owned_change,
    pct_started, adp, adp_change."""
    from espn_api.hockey.player import Player as EspnPlayer

    entries = data.get("players", []) if isinstance(data, Mapping) else list(data or [])
    out: list[dict[str, Any]] = []
    for raw in entries:
        pl = (raw or {}).get("player") or {}
        own = ownership_fields(pl)
        chg = own["pct_owned_change"]
        if pl.get("id") is None or chg is None or (chg >= 0 if fallers else chg <= 0):
            continue
        try:
            p = player_from_espn(EspnPlayer(raw), year, ownership=own)
        except Exception:  # noqa: BLE001 - one odd row never sinks the list
            continue
        out.append({"cid": p.cid, "espn_id": p.ids["espn"], "name": p.name, "team": p.team,
                    "positions": p.positions, "status": p.status, **own})
    out.sort(key=lambda d: d["pct_owned_change"], reverse=not fallers)
    return out[:max(0, int(limit))]


# -- league activity and daily lineups (harness capture) ----------------------

# kona_league_communication message types (espn_api ACTIVITY_MAP): adds, drops, trades.
ESPN_ACTIVITY_TYPES = (178, 180, 179, 239, 181, 244)
ESPN_ADD_TYPES = (178, 180)
ESPN_DROP_TYPES = (179, 181, 239)
ESPN_TRADE_TYPE = 244
ESPN_PAGE_SIZE = 25
ESPN_MAX_PAGES = 10


def _espn_ts(ms: Any) -> datetime | None:
    try:
        return datetime.fromtimestamp(float(ms) / 1000.0)
    except (TypeError, ValueError, OverflowError, OSError):
        return None


def parse_espn_activity(topics: Iterable[Mapping[str, Any]], team_names: Mapping[str, str] | None = None,
                        player_names: Mapping[Any, str] | None = None) -> list[ActivityItem]:
    """Raw ``kona_league_communication`` topics -> ActivityItems.

    Team of a message (as espn_api reads it): ``for`` for type 239, ``from`` (sender) and
    ``to`` (receiver) for trades (244), ``to`` otherwise. A trade message yields a TRADE_OUT
    for the sender and a TRADE_IN for the receiver; all items of one topic share its id as
    ``group_id``. Unknown message types are skipped."""
    names = {str(k): v for k, v in (team_names or {}).items()}
    pnames = player_names or {}
    out: list[ActivityItem] = []
    for topic in topics or []:
        ts = _espn_ts(topic.get("date"))
        if ts is None:
            continue
        gid = str(topic.get("id") or topic.get("date"))
        for msg in topic.get("messages") or []:
            mt = msg.get("messageTypeId")
            pid = msg.get("targetId")
            cid = f"espn:{pid}" if pid is not None else None
            pname = pnames.get(pid) or pnames.get(str(pid))

            def item(action: str, team: Any, other: Any = None) -> ActivityItem:
                tid = str(team) if team is not None else None
                return ActivityItem(source="espn", tx_id=gid, ts=ts, team_id=tid, team_name=names.get(tid or ""),
                                    action=action, cid=cid, player_name=pname, group_id=gid,  # type: ignore[arg-type]
                                    counterparty_id=str(other) if other is not None else None)

            if mt == ESPN_TRADE_TYPE:
                src, dst = msg.get("from"), msg.get("to")
                if src is not None:
                    out.append(item("TRADE_OUT", src, dst))
                if dst is not None:
                    out.append(item("TRADE_IN", dst, src))
            elif mt in ESPN_ADD_TYPES:
                out.append(item("ADD", msg.get("to", msg.get("for"))))
            elif mt in ESPN_DROP_TYPES:
                team = msg.get("for") if mt == 239 else msg.get("to", msg.get("for"))
                out.append(item("DROP", team if team is not None else msg.get("from")))
    return out


def parse_espn_box_lineups(box_scores: Iterable[Any], day: date) -> list[LineupDay]:
    """espn_api BoxScore objects (one scoring period) -> LineupDay rows for both teams."""
    out: list[LineupDay] = []
    for bs in box_scores or []:
        for team_attr, lineup_attr in (("home_team", "home_lineup"), ("away_team", "away_lineup")):
            team = getattr(bs, team_attr, None)
            if team in (None, 0, ""):
                continue
            tid = str(getattr(team, "team_id", team))
            for bp in getattr(bs, lineup_attr, None) or []:
                pid = getattr(bp, "playerId", None)
                if pid is None:
                    continue
                slot = SLOT_NAMES.get(getattr(bp, "slot_position", ""), "BN")
                pts = getattr(bp, "points", None)
                out.append(LineupDay(team_id=tid, date=day, cid=f"espn:{pid}", slot=slot,
                                     starting=slot not in ("BN", "IR"),
                                     provider_pts=float(pts) if isinstance(pts, (int, float)) else None))
    return out


class EspnProvider:
    def __init__(self, settings: Settings, cache: HttpCache, fa_size: int = 75):
        self.settings = settings
        self.cache = cache
        self.fa_size = fa_size
        self.warnings: list[str] = []
        self._league_obj: Any = None
        self._ctx: LeagueContext | None = None

    # -- best-effort extras (harness): failures become warnings, never exceptions ------
    def _warn(self, msg: str) -> None:
        self.warnings.append(msg)
        if self._ctx is not None:
            self._ctx.warnings.append(msg)

    def _league_cached(self) -> Any:
        if self._league_obj is None:
            self._league_obj = self._league()
        return self._league_obj

    def _activity_page(self, league: Any, size: int, offset: int) -> list[dict]:
        """One page of raw activity topics (espn_api League.recent_activity's request, but the
        raw JSON: it keeps the topic id, player ids and team ids)."""
        filters = {"topics": {"filterType": {"value": ["ACTIVITY_TRANSACTIONS"]}, "limit": size,
                              "limitPerMessageSet": {"value": 25}, "offset": offset,
                              "sortMessageDate": {"sortPriority": 1, "sortAsc": False},
                              "sortFor": {"sortPriority": 2, "sortAsc": False},
                              "filterIncludeMessageTypeIds": {"value": list(ESPN_ACTIVITY_TYPES)}}}
        data = league.espn_request.league_get(extend="/communication/",
                                              params={"view": "kona_league_communication"},
                                              headers={"x-fantasy-filter": json.dumps(filters)})
        return list((data or {}).get("topics") or [])

    def activity(self, since: date | None = None) -> list[ActivityItem]:
        """League adds / drops / trades, newest first, paging ``offset`` until items are older
        than ``since`` (at most ESPN_MAX_PAGES pages of ESPN_PAGE_SIZE). Best effort."""
        try:
            league = self._league_cached()
        except Exception as e:  # noqa: BLE001
            self._warn(f"ESPN activity unavailable: {e}")
            return []
        team_names = {str(t.team_id): t.team_name for t in getattr(league, "teams", []) or []}
        players = getattr(league, "player_map", None) or {}
        items: list[ActivityItem] = []
        seen: set[tuple] = set()
        for page in range(ESPN_MAX_PAGES):
            try:
                topics = self._activity_page(league, ESPN_PAGE_SIZE, page * ESPN_PAGE_SIZE)
                parsed = parse_espn_activity(topics, team_names, players)
            except Exception as e:  # noqa: BLE001
                self._warn(f"ESPN activity page {page + 1} unavailable: {type(e).__name__}: {e}")
                break
            for it in parsed:
                key = (it.tx_id, it.action, it.cid, it.team_id)
                if (since is not None and it.ts.date() < since) or key in seen:
                    continue
                seen.add(key)
                items.append(it)
            if len(topics) < ESPN_PAGE_SIZE:
                break
            stamps = [d for d in (_espn_ts(t.get("date")) for t in topics) if d is not None]
            if since is not None and stamps and min(stamps).date() < since:
                break
        return items

    def scoring_period_for(self, day: date, today: date | None = None) -> int | None:
        """ESPN hockey scoring period (one per day) of ``day``, from the league's current one."""
        try:
            cur = int(self._league_cached().current_week)
        except Exception as e:  # noqa: BLE001
            self._warn(f"ESPN scoring period unavailable: {e}")
            return None
        return cur - ((today or date.today()) - day).days

    def box_scores(self, scoring_period: int, day: date | None = None) -> list[LineupDay]:
        """Every team's lineup (slot, starting, ESPN points) for one scoring period (a day).
        ``day`` labels the rows (default: derived from the current scoring period). Best effort:
        [] with a warning on failure, [] for a period before the first one."""
        if scoring_period is None or scoring_period < 1:
            return []
        try:
            league = self._league_cached()
            boxes = league.box_scores(scoring_period=scoring_period, matchup_total=False)
            if day is None:
                day = date.today() - timedelta(days=int(league.current_week) - scoring_period)
            return parse_espn_box_lineups(boxes, day)
        except Exception as e:  # noqa: BLE001
            self._warn(f"ESPN box scores unavailable (period {scoring_period}): {type(e).__name__}: {e}")
            return []

    def trending(self, limit: int = 50, fallers: bool = False) -> list[dict[str, Any]]:
        """ESPN-wide % rostered risers (or fallers) as player-lite dicts (``parse_trending``)
        from the public leaguedefaults pool, through the HTTP cache (TTL 6h, no cookies). Covers
        players outside our free-agent pool; once a league is loaded each dict also gets
        ``in_league``: "fa", "rostered" or None (not in our league data). Best effort: [] with a
        warning on failure."""
        year = int(getattr(self.settings, "espn_year", None) or date.today().year + 1)
        try:
            data = self.cache.get_json(TRENDING_URL.format(year=year), params={"view": "kona_player_info"},
                                       headers={"x-fantasy-filter": json.dumps(trending_filter(year, fallers=fallers))},
                                       ttl=TRENDING_TTL)
            out = parse_trending(data, year, limit=limit, fallers=fallers)
        except Exception as e:  # noqa: BLE001
            self._warn(f"ESPN trending players unavailable: {type(e).__name__}: {e}")
            return []
        if self._ctx is not None:
            fa = {p.cid for p in self._ctx.free_agents}
            rostered = {p.cid for t in self._ctx.teams for p in t.players}
            for d in out:
                d["in_league"] = "fa" if d["cid"] in fa else "rostered" if d["cid"] in rostered else None
        return out

    def _league(self) -> Any:
        s = self.settings
        if not s.espn_league_id:
            raise ProviderError("ESPN_LEAGUE_ID is not set. Copy .env.example to .env and fill it in.")
        install_espn_cache(self.cache, ttl=ESPN_TTL)
        from espn_api.hockey import League
        from espn_api.requests.espn_requests import ESPNAccessDenied, ESPNInvalidLeague, ESPNUnknownError

        try:
            return League(league_id=int(s.espn_league_id), year=int(s.espn_year),
                          espn_s2=s.espn_s2, swid=s.espn_swid)
        except ESPNAccessDenied as e:
            hint = "" if (s.espn_s2 and s.espn_swid) else " ESPN_S2 and ESPN_SWID are required for private leagues."
            raise ProviderError(f"ESPN denied access: {e}.{hint} See README for how to copy the cookies.") from e
        except ESPNInvalidLeague as e:
            raise ProviderError(f"{e} (season {s.espn_year}). Check ESPN_LEAGUE_ID / ESPN_YEAR.") from e
        except ESPNUnknownError as e:
            raise ProviderError(f"ESPN request failed: {e}") from e
        except CacheMiss as e:
            raise ProviderError(f"Offline mode and ESPN data not cached: {e}") from e

    def _free_agents_raw(self, league: Any, position: str) -> list[tuple[Any, dict]]:
        """Mirror of League.free_agents that also keeps the raw JSON (for ownership %)."""
        from espn_api.hockey.constant import POSITION_MAP
        from espn_api.hockey.player import Player as EspnPlayer

        filters = {"players": {"filterStatus": {"value": ["FREEAGENT", "WAIVERS"]},
                               "filterSlotIds": {"value": [POSITION_MAP[position]]},
                               "limit": self.fa_size,
                               "sortPercOwned": {"sortPriority": 1, "sortAsc": False},
                               "sortDraftRanks": {"sortPriority": 100, "sortAsc": True, "value": "STANDARD"}}}
        params = {"view": "kona_player_info", "scoringPeriodId": league.current_week}
        data = league.espn_request.league_get(params=params, headers={"x-fantasy-filter": json.dumps(filters)})
        return [(EspnPlayer(raw), raw) for raw in data.get("players", [])]

    def load(self) -> LeagueContext:
        s = self.settings
        league = self._league()
        self._league_obj = league
        year = int(s.espn_year)
        try:
            raw_league = league.espn_request.get_league()  # cache hit: same request League just made
            raw_settings = league.espn_request.league_get(params={"view": "mSettings"})
            fa_raw = [pair for pos in FA_POSITIONS for pair in self._free_agents_raw(league, pos)]
        except CacheMiss as e:
            raise ProviderError(f"Offline mode and ESPN data not cached: {e}") from e

        owned = _ownership_from_league(raw_league)
        counts = raw_settings.get("settings", {}).get("rosterSettings", {}).get("lineupSlotCounts", {})

        mine = find_my_team(league.teams, s.espn_swid, s.espn_team)
        teams: list[FantasyTeam] = []
        for t in league.teams:
            slots = []
            for p in t.roster:
                slot = SLOT_NAMES.get(p.lineupSlot, "BN")
                slots.append(RosterSlot(slot=slot, player=player_from_espn(p, year, ownership=owned.get(p.playerId)),
                                        starting=slot not in ("BN", "IR")))
            teams.append(FantasyTeam(team_id=str(t.team_id), name=t.team_name, owner_is_me=t is mine,
                                     slots=slots, record=(t.wins, t.losses, t.ties)))

        fas: dict[str, Player] = {}
        for ep, raw in fa_raw:
            pl = player_from_espn(ep, year, ownership=ownership_fields(raw.get("player", {})))
            fas.setdefault(pl.cid, pl)

        st = league.settings
        self._ctx = LeagueContext(
            provider="espn",
            league_id=str(s.espn_league_id),
            season=year,
            name=st.name,
            scoring=scoring_from_settings(st.scoring_type, st._raw_scoring_settings.get("scoringItems", [])),
            roster_shape=roster_shape_from_counts(counts),
            teams=teams,
            free_agents=list(fas.values()),
            matchup_period=getattr(league, "currentMatchupPeriod", None),
            as_of=date.today(),
        )
        return self._ctx
