"""Per-player-season history table from the free NHL stats REST API.

One row per (player, regular season) with canonical counting stats, games played, age on
Oct 1 of the season and position group (F / D / G). Built from six season-level reports per
season (skater summary / realtime / faceoffwins / bios, goalie summary / bios), so a full
rebuild of 16 seasons is ~100 requests.

In-season checkpoints use the same reports restricted by ``gameDate`` (the API supports
date-range filters), which gives season-to-date, last-30/15/7-day and rest-of-season lines for
*every* player, hits and blocks included, in ~15 requests per checkpoint instead of hundreds of
per-player game logs.

Storage is plain JSON under ``<fm_data_dir>/backtest/`` (pyarrow is not installed and the table
is only a few MB): ``player_seasons.json`` and ``windows.json``. Both are incremental: seasons /
checkpoints already stored are skipped unless ``force``.
"""
from __future__ import annotations

import json
import time
from dataclasses import asdict, dataclass
from datetime import date, timedelta
from pathlib import Path
from typing import Any, Callable, Iterable, Mapping

from ..models import Player, StatLine, normalize_name
from ..providers.nhl import NhlClient, _goalie_from_row, _skater_from_rows, current_season

FIRST_SEASON = 20102011
LAST_SEASON = 20252026
DAY = 86400.0
TTL_HISTORICAL = 30 * DAY   # finished seasons never change
TTL_CURRENT = 0.5 * DAY

SKATER_KEYS = ("G", "A", "PTS", "PM", "PIM", "PPG", "PPA", "PPP", "SHG", "SHA", "SHP", "GWG",
               "SOG", "HIT", "BLK", "FOW", "FOL", "ENG")
GOALIE_KEYS = ("GS", "W", "L", "OTL", "GA", "SA", "SV", "SO", "G", "A", "PIM")

# In-season checkpoints (month, day) and the windows stored for each one.
CHECKPOINTS = ((11, 1), (12, 1), (1, 1))
WINDOWS = ("to_date", "last30", "last15", "last7", "rest")
WINDOW_DAYS = {"last30": 30, "last15": 15, "last7": 7}


# --------------------------------------------------------------------------- seasons

def season_start_year(season: int) -> int:
    return season // 10000


def season_label(season: int) -> str:
    y = season_start_year(season)
    return f"{y}-{str(y + 1)[-2:]}"


def next_season(season: int) -> int:
    return season + 10001


def prev_season(season: int) -> int:
    return season - 10001


def season_range(first: int, last: int) -> list[int]:
    out, s = [], first
    while s <= last:
        out.append(s)
        s = next_season(s)
    return out


def parse_seasons(spec: str | None, default: Iterable[int]) -> list[int]:
    """'20162017-20252026', '20232024,20242025', '2016-2025' (start years) -> season ids."""
    if not spec:
        return list(default)

    def one(tok: str) -> int:
        tok = tok.strip()
        if len(tok) == 4:
            y = int(tok)
            return y * 10000 + y + 1
        return int(tok)

    out: list[int] = []
    for part in spec.split(","):
        part = part.strip()
        if not part:
            continue
        if "-" in part:
            a, b = part.split("-", 1)
            out.extend(season_range(one(a), one(b)))
        else:
            out.append(one(part))
    return sorted(set(out))


def age_on_oct1(birth_date: str | date | None, season: int) -> float | None:
    if not birth_date:
        return None
    bd = birth_date if isinstance(birth_date, date) else date.fromisoformat(str(birth_date)[:10])
    return round((date(season_start_year(season), 10, 1) - bd).days / 365.25, 3)


def position_group(position: str | None) -> str:
    if position == "G":
        return "G"
    if position == "D":
        return "D"
    return "F"


# --------------------------------------------------------------------------- rows

@dataclass
class PlayerSeason:
    player_id: int
    season: int
    name: str
    group: str                  # F / D / G
    position: str | None        # C / LW / RW / D / G
    gp: int
    stats: dict[str, float]     # season totals (no GP key)
    team: str | None = None
    birth_date: str | None = None
    age: float | None = None    # on Oct 1 of the season's start year

    @property
    def is_goalie(self) -> bool:
        return self.group == "G"

    def per_game(self) -> dict[str, float]:
        if self.gp <= 0:
            return {}
        return {k: v / self.gp for k, v in self.stats.items()}

    def statline(self, split: str = "prior") -> StatLine:
        return StatLine(split=split, gp=self.gp, stats={**self.stats, "GP": float(self.gp)})

    def to_player(self, split: str = "prior") -> Player:
        """App-side Player carrying this row as its ``split`` line (for valuation functions)."""
        return Player(cid=f"nhl:{self.player_id}", name=self.name, name_norm=normalize_name(self.name),
                      ids={"nhl": str(self.player_id)}, team=self.team,
                      positions=[self.position or ("G" if self.is_goalie else "C")],
                      lines={split: self.statline(split)} if self.gp > 0 else {})

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_dict(cls, d: Mapping[str, Any]) -> "PlayerSeason":
        return cls(player_id=int(d["player_id"]), season=int(d["season"]), name=d.get("name") or "",
                   group=d.get("group") or "F", position=d.get("position"), gp=int(d.get("gp") or 0),
                   stats={k: float(v) for k, v in (d.get("stats") or {}).items()}, team=d.get("team"),
                   birth_date=d.get("birth_date"), age=d.get("age"))


@dataclass
class WindowLine:
    """Stats of one player over a date window (gp + totals)."""
    gp: int
    stats: dict[str, float]

    def per_game(self) -> dict[str, float]:
        return {k: v / self.gp for k, v in self.stats.items()} if self.gp > 0 else {}

    def statline(self, split: str) -> StatLine:
        return StatLine(split=split, gp=self.gp, stats={**self.stats, "GP": float(self.gp)})


def _skater_stats(summary: dict, realtime: dict | None, faceoffs: dict | None) -> tuple[int, dict[str, float], str | None, str | None]:
    row = _skater_from_rows(summary, realtime, faceoffs)
    st = dict(row.stats)
    for k in ("HIT", "BLK", "ENG"):   # no realtime row -> zero, not missing
        st.setdefault(k, 0.0)
    if "PPP" in st and "PPG" in st:
        st["PPA"] = st["PPP"] - st["PPG"]
    if "SHP" in st and "SHG" in st:
        st["SHA"] = st["SHP"] - st["SHG"]
    gp = int(st.get("GP", 0))
    stats = {k: float(st[k]) for k in SKATER_KEYS if k in st}
    return gp, stats, row.position, row.team


def _goalie_stats(row: dict) -> tuple[int, dict[str, float], str | None]:
    g = _goalie_from_row(row)
    st = dict(g.stats)
    for src, canon in (("goals", "G"), ("assists", "A"), ("penaltyMinutes", "PIM")):
        v = row.get(src)
        st[canon] = float(v) if isinstance(v, (int, float)) and not isinstance(v, bool) else 0.0
    gp = int(st.get("GP", 0))
    return gp, {k: float(st[k]) for k in GOALIE_KEYS if k in st}, g.team


# --------------------------------------------------------------------------- fetching

class CountingFetcher:
    """fetch_json(url, params) through HttpCache with a long TTL for finished seasons.

    Counts network requests vs cache hits and the wall time spent on the network."""

    def __init__(self, cache: Any, today: date | None = None, pause: float = 0.0):
        self.cache = cache
        self.today = today or date.today()
        self.pause = pause
        self.network = 0
        self.cached = 0
        self.seconds = 0.0

    def ttl_for(self, params: Mapping[str, Any] | None) -> float:
        cay = str((params or {}).get("cayenneExp", ""))
        cur = current_season(self.today)
        for tok in cay.replace("=", " ").split():
            if tok.isdigit() and len(tok) == 8:
                return TTL_CURRENT if int(tok) >= cur else TTL_HISTORICAL
        return TTL_CURRENT

    def __call__(self, url: str, params: dict | None = None) -> Any:
        t0 = time.perf_counter()
        resp = self.cache.fetch(url, params=params, ttl=self.ttl_for(params))
        if resp.from_cache:
            self.cached += 1
        else:
            self.network += 1
            self.seconds += time.perf_counter() - t0
            if self.pause:
                time.sleep(self.pause)
        if not 200 <= resp.status_code < 300:
            raise RuntimeError(f"HTTP {resp.status_code} for {url}")
        return resp.json()

    def summary(self) -> str:
        return f"{self.network} network requests ({self.seconds:.1f}s), {self.cached} cache hits"


def _report(client: NhlClient, kind: str, report: str, season: int, extra: str | None = None) -> list[dict]:
    return client._report(kind, report, season, 2, extra)


def fetch_season(client: NhlClient, season: int) -> list[PlayerSeason]:
    """All skater and goalie rows of one regular season."""
    births: dict[int, str] = {}
    for kind in ("skater", "goalie"):
        for r in _report(client, kind, "bios", season):
            if r.get("birthDate"):
                births[int(r["playerId"])] = str(r["birthDate"])[:10]
    realtime = {r["playerId"]: r for r in _report(client, "skater", "realtime", season)}
    try:
        faceoffs = {r["playerId"]: r for r in _report(client, "skater", "faceoffwins", season)}
    except Exception:
        faceoffs = {}
    out: list[PlayerSeason] = []
    for r in _report(client, "skater", "summary", season):
        pid = int(r["playerId"])
        gp, stats, pos, team = _skater_stats(r, realtime.get(pid, {}), faceoffs.get(pid))
        bd = births.get(pid)
        out.append(PlayerSeason(player_id=pid, season=season, name=r.get("skaterFullName") or "",
                                group=position_group(pos), position=pos, gp=gp, stats=stats, team=team,
                                birth_date=bd, age=age_on_oct1(bd, season)))
    for r in _report(client, "goalie", "summary", season):
        pid = int(r["playerId"])
        gp, stats, team = _goalie_stats(r)
        bd = births.get(pid)
        out.append(PlayerSeason(player_id=pid, season=season, name=r.get("goalieFullName") or "",
                                group="G", position="G", gp=gp, stats=stats, team=team,
                                birth_date=bd, age=age_on_oct1(bd, season)))
    return out


def fetch_window(client: NhlClient, season: int, start: date | None, end: date | None,
                 faceoffs: bool = False) -> dict[int, WindowLine]:
    """player_id -> WindowLine for games in [start, end] (inclusive) of a regular season.
    ``faceoffs`` also fetches the faceoffwins report (FOW / FOL), best effort."""
    parts = []
    if start is not None:
        parts.append(f'gameDate>="{start.isoformat()}"')
    if end is not None:
        parts.append(f'gameDate<="{end.isoformat()}"')
    extra = " and ".join(parts) or None
    realtime = {r["playerId"]: r for r in _report(client, "skater", "realtime", season, extra)}
    fo: dict[Any, dict] = {}
    if faceoffs:
        try:
            fo = {r["playerId"]: r for r in _report(client, "skater", "faceoffwins", season, extra)}
        except Exception:
            fo = {}
    out: dict[int, WindowLine] = {}
    for r in _report(client, "skater", "summary", season, extra):
        gp, stats, _, _ = _skater_stats(r, realtime.get(r["playerId"], {}), fo.get(r["playerId"]))
        if gp > 0:
            out[int(r["playerId"])] = WindowLine(gp, stats)
    for r in _report(client, "goalie", "summary", season, extra):
        gp, stats, _ = _goalie_stats(r)
        if gp > 0:
            out[int(r["playerId"])] = WindowLine(gp, stats)
    return out


def checkpoint_dates(season: int) -> list[date]:
    y = season_start_year(season)
    return [date(y if m >= 9 else y + 1, m, d) for m, d in CHECKPOINTS]


def fetch_checkpoint(client: NhlClient, season: int, day: date) -> dict[str, dict[int, WindowLine]]:
    """Windows around checkpoint ``day``: to_date / last30 / last15 / last7 end the day before,
    rest starts on ``day``."""
    end = day - timedelta(days=1)
    out = {"to_date": fetch_window(client, season, None, end)}
    for name, days in WINDOW_DAYS.items():
        out[name] = fetch_window(client, season, day - timedelta(days=days), end)
    out["rest"] = fetch_window(client, season, day, None)
    return out


# --------------------------------------------------------------------------- storage

def backtest_dir(data_dir: Path | str) -> Path:
    p = Path(data_dir) / "backtest"
    p.mkdir(parents=True, exist_ok=True)
    return p


class SeasonTable:
    """All stored player-season rows, indexed by season and by player."""

    def __init__(self, rows: Iterable[PlayerSeason] = ()):
        self.rows: list[PlayerSeason] = list(rows)
        self._reindex()

    def _reindex(self) -> None:
        self.by_season: dict[int, dict[int, PlayerSeason]] = {}
        self.by_player: dict[int, dict[int, PlayerSeason]] = {}
        for r in self.rows:
            self.by_season.setdefault(r.season, {})[r.player_id] = r
            self.by_player.setdefault(r.player_id, {})[r.season] = r

    @property
    def seasons(self) -> list[int]:
        return sorted(self.by_season)

    def add(self, rows: Iterable[PlayerSeason]) -> None:
        rows = list(rows)
        drop = {(r.season, r.player_id) for r in rows}
        self.rows = [r for r in self.rows if (r.season, r.player_id) not in drop] + rows
        self._reindex()

    def get(self, player_id: int, season: int) -> PlayerSeason | None:
        return self.by_season.get(season, {}).get(player_id)

    def history(self, player_id: int, before: int) -> list[PlayerSeason]:
        """The player's rows for seasons < ``before``, oldest first."""
        seasons = self.by_player.get(player_id, {})
        return [seasons[s] for s in sorted(seasons) if s < before]

    def age(self, player_id: int, season: int) -> float | None:
        """Age on Oct 1 of ``season`` from any stored birth date of the player."""
        for r in self.by_player.get(player_id, {}).values():
            if r.birth_date:
                return age_on_oct1(r.birth_date, season)
        return None


def table_path(data_dir: Path | str) -> Path:
    return backtest_dir(data_dir) / "player_seasons.json"


def load_table(data_dir: Path | str) -> SeasonTable:
    p = table_path(data_dir)
    if not p.exists():
        return SeasonTable()
    d = json.loads(p.read_text(encoding="utf-8"))
    return SeasonTable(PlayerSeason.from_dict(r) for r in d.get("rows") or [])


def save_table(table: SeasonTable, data_dir: Path | str) -> Path:
    p = table_path(data_dir)
    payload = {"version": 1, "seasons": table.seasons,
               "rows": [r.to_dict() for r in sorted(table.rows, key=lambda r: (r.season, r.player_id))]}
    tmp = p.with_suffix(".tmp")
    tmp.write_text(json.dumps(payload, separators=(",", ":")), encoding="utf-8")
    tmp.replace(p)
    return p


def build_table(client: NhlClient, data_dir: Path | str, seasons: Iterable[int], force: bool = False,
                log: Callable[[str], None] | None = None) -> SeasonTable:
    """Fetch missing seasons into the stored table (sequential; each season saved as it lands)."""
    table = load_table(data_dir)
    have = set(table.seasons)
    for s in seasons:
        if s in have and not force:
            continue
        rows = fetch_season(client, s)
        table.add(rows)
        save_table(table, data_dir)
        if log:
            log(f"{season_label(s)}: {sum(1 for r in rows if not r.is_goalie)} skaters, "
                f"{sum(1 for r in rows if r.is_goalie)} goalies")
    return table


# ---- in-season windows ---------------------------------------------------------

Windows = dict[int, dict[str, dict[str, dict[int, WindowLine]]]]  # season -> day -> window -> pid -> line


def windows_path(data_dir: Path | str) -> Path:
    return backtest_dir(data_dir) / "windows.json"


def load_windows(data_dir: Path | str) -> Windows:
    p = windows_path(data_dir)
    if not p.exists():
        return {}
    raw = json.loads(p.read_text(encoding="utf-8"))
    out: Windows = {}
    for s, days in raw.items():
        for day, wins in days.items():
            for w, rows in wins.items():
                out.setdefault(int(s), {}).setdefault(day, {})[w] = {
                    int(pid): WindowLine(int(v["gp"]), {k: float(x) for k, x in v["stats"].items()})
                    for pid, v in rows.items()}
    return out


def save_windows(windows: Windows, data_dir: Path | str) -> Path:
    p = windows_path(data_dir)
    raw = {str(s): {day: {w: {str(pid): {"gp": ln.gp, "stats": ln.stats} for pid, ln in rows.items()}
                          for w, rows in wins.items()} for day, wins in days.items()}
           for s, days in windows.items()}
    tmp = p.with_suffix(".tmp")
    tmp.write_text(json.dumps(raw, separators=(",", ":")), encoding="utf-8")
    tmp.replace(p)
    return p


def build_windows(client: NhlClient, data_dir: Path | str, seasons: Iterable[int], force: bool = False,
                  today: date | None = None, log: Callable[[str], None] | None = None) -> Windows:
    """Fetch checkpoint windows (Nov 1 / Dec 1 / Jan 1) for each season; skips stored ones and
    checkpoints that are still in the future."""
    today = today or date.today()
    windows = load_windows(data_dir)
    for s in seasons:
        for day in checkpoint_dates(s):
            if day >= today:
                continue
            key = day.isoformat()
            if key in windows.get(s, {}) and not force:
                continue
            windows.setdefault(s, {})[key] = fetch_checkpoint(client, s, day)
            save_windows(windows, data_dir)
            if log:
                td = windows[s][key]["to_date"]
                log(f"{season_label(s)} checkpoint {key}: {len(td)} players with games to date")
    return windows
