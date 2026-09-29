"""Fill line / unit / goalie-start fields on a LeagueContext from Daily Faceoff.

:func:`enrich_lines` (best-effort: every failure becomes a ``ctx.warnings`` entry):

1. fetch every team's line-combinations page (12 h cache) and today's starting goalies (3 h)
2. map Daily Faceoff player ids to NHL ids through the crosswalk (source ``"dfo"``; NHL
   candidates = league players that already carry NHL ids plus current NHL rosters)
3. set ``Player.line`` / ``pp_unit`` / ``pk_unit`` for league players with NHL ids, and a
   ``status_note`` from Daily Faceoff's injury / game-time-decision tag when the league
   provider gave none
4. persist a daily snapshot (``<fm_data_dir>/lines/lines-YYYY-MM-DD.json``) and set
   ``Player.line_change`` against the most recent earlier snapshot: "PP2 -> PP1",
   "F3 -> F1", "G2 -> G1", "F2 -> IR", "scratched", "new to lineup"
5. goalies: ``confirmed_start`` True for the named starter (Confirmed / Likely / Expected),
   False for his team's other goalies, None when unknown; ``start_source`` says who reported
   it and when, e.g. "DFO Confirmed: Jeremy Swayman starts NYR@BOS (Jack Studley, Sep 29
   15:02 UTC)"
"""
from __future__ import annotations

import json
import logging
import re
from datetime import date, datetime, timezone
from pathlib import Path
from typing import Any, Iterable, Mapping

from pydantic import BaseModel, Field

from ..matching.crosswalk import Crosswalk, SourceItem, nhl_candidates
from ..matching.matcher import Candidate, PlayerIndex
from ..matching.normalize import normalize_name, normalize_team
from ..models import LeagueContext, Player
from .dailyfaceoff import (TEAM_SLUGS, DailyFaceoffClient, GoalieStart, LinePlayer, TeamLines,
                           make_cached_fetch_text)

log = logging.getLogger(__name__)

SOURCE = "dfo"
LINES_DIR = "lines"
_FILE_RE = re.compile(r"^lines-(\d{4}-\d{2}-\d{2})\.json$")
NO_PP = "no PP"


# --------------------------------------------------------------------------- snapshots

class LineSnapshotStore:
    """Daily JSON snapshots under ``<data_dir>/lines/lines-YYYY-MM-DD.json``."""

    def __init__(self, data_dir: Path | str):
        self.dir = Path(data_dir) / LINES_DIR

    def path(self, day: date) -> Path:
        return self.dir / f"lines-{day.isoformat()}.json"

    def dates(self) -> list[date]:
        if not self.dir.is_dir():
            return []
        out = []
        for f in self.dir.iterdir():
            m = _FILE_RE.match(f.name)
            if m:
                try:
                    out.append(date.fromisoformat(m.group(1)))
                except ValueError:
                    continue
        return sorted(out)

    def load(self, day: date) -> dict | None:
        try:
            return json.loads(self.path(day).read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return None

    def save(self, day: date, snapshot: Mapping[str, Any]) -> Path:
        self.dir.mkdir(parents=True, exist_ok=True)
        p = self.path(day)
        tmp = p.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(snapshot, indent=1, sort_keys=True, default=str), encoding="utf-8")
        tmp.replace(p)
        return p

    def previous(self, day: date) -> tuple[date, dict] | None:
        """Most recent snapshot strictly before ``day``."""
        for d in reversed(self.dates()):
            if d < day:
                snap = self.load(d)
                if snap is not None:
                    return d, snap
        return None


def _player_key(lp: LinePlayer, team: str) -> str:
    return str(lp.dfo_id) if lp.dfo_id is not None else f"{team}:{normalize_name(lp.name)}"


def build_snapshot(day: date, lines: Mapping[str, TeamLines], starts: Iterable[GoalieStart] = (),
                   nhl_ids: Mapping[str, int] | None = None) -> dict:
    """JSON-able snapshot: teams (update time / source), players keyed by DFO id, starts."""
    nhl_ids = nhl_ids or {}
    players: dict[str, dict] = {}
    teams: dict[str, dict] = {}
    for team, tl in sorted(lines.items()):
        teams[team] = {"updated_at": tl.updated_at.isoformat() if tl.updated_at else None,
                       "source": tl.source, "source_url": tl.source_url}
        for lp in tl.players:
            key = _player_key(lp, team)
            players[key] = {
                "name": lp.name, "team": team, "dfo_id": lp.dfo_id,
                "nhl_id": nhl_ids.get(str(lp.dfo_id)) if lp.dfo_id is not None else None,
                "line": lp.line, "group": lp.group, "goalie_depth": lp.goalie_depth,
                "pp_unit": lp.pp_unit, "pk_unit": lp.pk_unit,
                "injury_status": lp.injury_status, "gtd": lp.gtd,
            }
    return {
        "date": day.isoformat(),
        "created": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "teams": teams,
        "players": players,
        "starts": [s.model_dump(mode="json") for s in starts],
    }


def _unit(u: str | None) -> str:
    return u.upper() if u else NO_PP


def _line_label(rec: Mapping[str, Any]) -> str:
    if rec.get("line") == "g" and rec.get("goalie_depth"):
        return f"G{rec['goalie_depth']}"
    return str(rec.get("line") or "").upper()


def diff_snapshots(prev: Mapping[str, Any] | None, cur: Mapping[str, Any]) -> dict[str, str]:
    """{player key: change text} between two snapshots. Only teams present in both
    snapshots are compared (a team whose page failed on either day reports nothing)."""
    if not prev:
        return {}
    both = set(prev.get("teams") or {}) & set(cur.get("teams") or {})
    pp_, cp_ = prev.get("players") or {}, cur.get("players") or {}
    out: dict[str, str] = {}
    for key in sorted(set(pp_) | set(cp_)):
        c, p = cp_.get(key), pp_.get(key)
        changes: list[str] = []
        if c is not None and c.get("team") in both:
            same_team = p is not None and p.get("team") == c.get("team")
            if p is not None and not same_team and p.get("team") in both:
                changes.append(f"{p.get('team')} -> {c.get('team')}")
            prev_in = bool(p and p.get("line")) and same_team
            cur_in = bool(c.get("line"))
            if cur_in and not prev_in:
                changes.append("new to lineup")
            elif prev_in and cur_in:
                a, b = _line_label(p), _line_label(c)
                if a != b:
                    changes.append(f"{a} -> {b}")
            elif prev_in and not cur_in:
                changes.append(f"{_line_label(p)} -> IR" if c.get("group") == "ir" else "scratched")
            before = p.get("pp_unit") if same_team else None
            if before != c.get("pp_unit") and (same_team or c.get("pp_unit")):
                changes.append(f"{_unit(before)} -> {_unit(c.get('pp_unit'))}")
        elif c is None and p is not None and p.get("line") and p.get("team") in both:
            changes.append("scratched")
        if changes:
            out[key] = "; ".join(changes)
    return out


# --------------------------------------------------------------------------- helpers

# Per-date team metadata ({team: {"source", "source_url", "updated_at"}}) of the latest
# enrich_lines run in this process, so recommend.alerts can cite who reported a line change.
_TEAM_META: dict[date, dict[str, dict[str, Any]]] = {}


def team_meta_for(day: date) -> dict[str, dict[str, Any]]:
    return _TEAM_META.get(day, {})

class LinesResult(BaseModel):
    day: date
    teams: int = 0                        # team pages parsed
    listed: int = 0                       # distinct players on those pages
    matched: int = 0                      # of which mapped to NHL ids
    pending: int = 0
    applied: int = 0                      # league players updated
    changes: dict[str, str] = Field(default_factory=dict)       # league player name -> line_change
    starts: list[GoalieStart] = Field(default_factory=list)
    starters_applied: int = 0             # league goalies with confirmed_start set
    snapshot_path: str | None = None
    previous_date: date | None = None
    warnings: list[str] = Field(default_factory=list)


def _short(e: Exception) -> str:
    s = str(e).strip().splitlines()[0] if str(e).strip() else type(e).__name__
    return s[:120]


def _league_candidates(players: Iterable[Player]) -> list[Candidate]:
    out = []
    for p in players:
        if p.nhl_id is None:
            continue
        pos = [x for x in p.positions if x not in ("F", "UTIL")] or p.positions
        out.append(Candidate(key=p.nhl_id, name=p.name, team=normalize_team(p.team),
                             position="/".join(pos) if pos else None))
    return out


def _roster_candidates(cache: Any, teams: Iterable[str], as_of: date, warnings: list[str]) -> list[Any]:
    from .enrich import make_fetch_json
    from .nhl import NhlClient, current_season

    client = NhlClient(fetch_json=make_fetch_json(cache), season=current_season(as_of))
    out: list[Any] = []
    failed = 0
    last: Exception | None = None
    for t in teams:
        try:
            out.extend(client.team_roster(t))
        except Exception as e:
            failed += 1
            last = e
    if failed and last is not None:
        warnings.append(f"NHL rosters for Daily Faceoff matching: {failed} team(s) unavailable ({_short(last)})")
    return out


def _resolve(items: list[SourceItem], index: PlayerIndex, crosswalk: Crosswalk | None
             ) -> tuple[dict[str, int], int]:
    """{dfo id: nhl id}, pending count."""
    if crosswalk is not None:
        ids, stats = crosswalk.resolve_source(SOURCE, items, index)
        return ids, stats.pending
    out: dict[str, int] = {}
    pending = 0
    for it in items:
        m = index.match(it.name, team=it.team, position=it.position)
        if m.matched:
            out[it.source_id] = int(m.key)
        elif m.confidence == "pending":
            pending += 1
    return out, pending


def _dfo_note(lp: LinePlayer) -> str | None:
    parts = []
    if lp.injury_status:
        parts.append(lp.injury_status)
    if lp.gtd:
        parts.append("game-time decision")
    return f"DFO: {', '.join(parts)}" if parts else None


def _fmt_time(dt: datetime | None) -> str:
    if dt is None:
        return "time n/a"
    return dt.astimezone(timezone.utc).strftime("%b %d %H:%M UTC")


def start_source_text(s: GoalieStart) -> str:
    """'DFO Confirmed: Jeremy Swayman starts NYR@BOS (Jack Studley, Sep 29 15:02 UTC)'."""
    who = s.source or "Daily Faceoff"
    return f"DFO {s.strength}: {s.goalie_name} starts {s.game} ({who}, {_fmt_time(s.created_at)})"


# --------------------------------------------------------------------------- main entry

def enrich_lines(ctx: LeagueContext, cache: Any, crosswalk: Crosswalk | None, as_of: date | None = None,
                 snapshot_store: LineSnapshotStore | Path | str | None = None, *,
                 client: DailyFaceoffClient | None = None,
                 candidates: list[Candidate] | PlayerIndex | None = None,
                 teams: Iterable[str] | None = None, fetch_rosters: bool = True) -> LinesResult:
    """Mutates the league players in ``ctx``; never raises for a failing source.

    ``cache`` (HttpCache) feeds both Daily Faceoff (12 h / 3 h TTLs) and, unless
    ``candidates`` is given, the NHL roster lookups used as match candidates. ``crosswalk``
    None matches without persisting. ``snapshot_store`` (a store or the fm data dir) enables
    the daily snapshot and ``line_change``."""
    day = as_of or ctx.as_of
    res = LinesResult(day=day)
    if client is None:
        client = DailyFaceoffClient(fetch_text=make_cached_fetch_text(cache) if cache is not None else None)
    store = snapshot_store if isinstance(snapshot_store, LineSnapshotStore) or snapshot_store is None \
        else LineSnapshotStore(snapshot_store)
    team_list = [normalize_team(t) or t for t in (teams if teams is not None else TEAM_SLUGS)]

    # 1. fetch ----------------------------------------------------------------------
    try:
        lines = client.all_lines(team_list)
    except Exception as e:
        lines = {}
        res.warnings.append(f"Daily Faceoff lines unavailable ({_short(e)})")
    res.warnings.extend(client.warnings)
    client.warnings.clear()
    starts: list[GoalieStart] = []
    try:
        starts = client.starting_goalies(day)
    except Exception as e:
        res.warnings.append(f"Daily Faceoff starting goalies unavailable ({_short(e)})")
    res.starts = starts
    res.teams = len(lines)

    # 2. crosswalk --------------------------------------------------------------------
    players = ctx.all_players()
    items: list[SourceItem] = []
    for team, tl in lines.items():
        for lp in tl.players:
            if lp.dfo_id is not None:
                items.append(SourceItem(source_id=str(lp.dfo_id), name=lp.name, team=team, position=lp.position))
    for s in starts:
        if s.dfo_id is not None:
            items.append(SourceItem(source_id=str(s.dfo_id), name=s.goalie_name, team=s.team, position="G"))
    line_ids = {str(lp.dfo_id) for tl in lines.values() for lp in tl.players if lp.dfo_id is not None}
    res.listed = len(line_ids)
    dfo_to_nhl: dict[str, int] = {}
    if items:
        try:
            if candidates is None:
                cands_src: list[Any] = [_league_candidates(players)]
                if fetch_rosters and cache is not None:
                    cands_src.append(_roster_candidates(cache, sorted(set(lines) | {s.team for s in starts}),
                                                        day, res.warnings))
                index = PlayerIndex(nhl_candidates(*cands_src))
            else:
                index = candidates if isinstance(candidates, PlayerIndex) else PlayerIndex(candidates)
            dfo_to_nhl, res.pending = _resolve(items, index, crosswalk)
        except Exception as e:
            res.warnings.append(f"Daily Faceoff player matching failed ({_short(e)})")
    res.matched = sum(1 for k in line_ids if k in dfo_to_nhl)

    if lines:
        _TEAM_META[day] = {t: {"source": tl.source, "source_url": tl.source_url, "updated_at": tl.updated_at}
                           for t, tl in lines.items()}

    # 3. snapshot + diff -------------------------------------------------------------------
    changes: dict[str, str] = {}
    if lines:
        snap = build_snapshot(day, lines, starts, dfo_to_nhl)
        if store is not None:
            try:
                prev = store.previous(day)
                if prev is not None:
                    res.previous_date = prev[0]
                    changes = diff_snapshots(prev[1], snap)
                snap["changes"] = changes
                res.snapshot_path = str(store.save(day, snap))
            except Exception as e:
                res.warnings.append(f"line snapshot failed ({_short(e)})")

    # 4. apply to league players --------------------------------------------------------------
    by_nhl: dict[int, list[Player]] = {}
    for p in players:
        if p.nhl_id is not None:
            by_nhl.setdefault(p.nhl_id, []).append(p)
    for team, tl in lines.items():
        for lp in tl.players:
            nid = dfo_to_nhl.get(str(lp.dfo_id)) if lp.dfo_id is not None else None
            for p in by_nhl.get(nid, []) if nid is not None else []:
                p.line = lp.line
                p.pp_unit = lp.pp_unit
                p.pk_unit = lp.pk_unit
                ch = changes.get(_player_key(lp, team))
                p.line_change = ch
                if ch:
                    res.changes[p.name] = ch
                note = _dfo_note(lp)
                if note and not p.status_note:
                    p.status_note = note
                res.applied += 1
    # players who vanished from their team's page since the last snapshot
    for key, ch in changes.items():
        if ch != "scratched" or not key.isdigit():
            continue
        nid = dfo_to_nhl.get(key)
        if nid is None and store is not None and res.previous_date is not None:
            prev_snap = store.load(res.previous_date) or {}
            nid = ((prev_snap.get("players") or {}).get(key) or {}).get("nhl_id")
        for p in by_nhl.get(int(nid), []) if nid is not None else []:
            if p.line_change is None:
                p.line_change = ch
                res.changes[p.name] = ch

    # 5. goalie starts ------------------------------------------------------------------------
    starters: dict[str, GoalieStart] = {}
    for s in starts:
        if s.is_start:
            prior = starters.get(s.team)
            if prior is None or (s.is_confirmed and not prior.is_confirmed):
                starters[s.team] = s
    for p in players:
        if not p.is_goalie:
            continue
        team = normalize_team(p.team)
        s = starters.get(team) if team else None
        if s is None:
            continue
        sid = dfo_to_nhl.get(str(s.dfo_id)) if s.dfo_id is not None else None
        is_him = (sid is not None and p.nhl_id == sid) or \
                 (sid is None and normalize_name(p.name) == normalize_name(s.goalie_name))
        p.confirmed_start = bool(is_him)
        p.start_source = start_source_text(s)
        res.starters_applied += 1

    # 6. notes --------------------------------------------------------------------------------
    if lines or starts:
        n_start = sum(1 for s in starts if s.is_start)
        n_conf = sum(1 for s in starts if s.is_confirmed)
        prev_txt = f" vs {res.previous_date}" if res.previous_date else ""
        ctx.source_notes.append(
            f"Daily Faceoff: lines for {res.teams} teams ({res.matched}/{res.listed} players matched to NHL ids, "
            f"{res.applied} league players updated, {len(res.changes)} line changes{prev_txt}); "
            f"goalies {day}: {n_conf} confirmed / {n_start} named starters in {len({s.game for s in starts})} games")
        if res.pending:
            ctx.source_notes.append(f"Daily Faceoff: {res.pending} player matches pending (fm sync --review)")
    ctx.warnings.extend(res.warnings)
    return res


__all__ = ["LineSnapshotStore", "LinesResult", "build_snapshot", "diff_snapshots", "enrich_lines",
           "start_source_text", "team_meta_for"]
