"""Pre-game afternoon run (``fm harness pregame``): what changed since this morning.

Daily-lineup leagues are decided around 5 PM ET: Daily Faceoff confirms starting goalies through
the afternoon, late scratches and injuries land, a call-up jumps onto a top line. The morning run
(``fm harness daily`` + ``fm report``) cannot see any of that, so this run:

1. force-refreshes the sources that move during the day: the Daily Faceoff starting-goalies page
   and the ESPN injury feed bypass the HTTP cache (``ttl=0``; the fresh copy is stored, so the
   league load that follows reads it through its normal TTLs), and the line pages of tonight's
   teams are refetched only when the cached copy is older than 6 h;
2. loads the league with the same building blocks as ``daily`` (``cli_backtest._load_league_full``);
3. recomputes the week lineup recs, today's optimal lineup (players with a game today, per-game
   value x availability x goalie start probability), line / role alerts and injury alerts;
4. diffs a compact per-league state against the previous snapshot of the day
   (``<fm_data_dir>/pregame/<league>-<YYYY-MM-DD>.json``; ``daily`` writes one at the end of each
   league load, so the first pre-game run compares with the morning) and reports only what
   changed, typed: ``goalie_confirmed`` (scope ``mine`` / ``opponent`` = the goalie my skaters
   face tonight / ``streamer`` = a free agent), ``my_player_status_change``, ``injury_new``,
   ``lineup_change`` (a start / sit that differs from the morning's optimal lineup for today) and
   ``new_alert`` (my players' line / PP-unit moves, new line alerts, scratches);
5. writes the new snapshot (the next run the same day diffs against it).

Without a snapshot for today the baseline statuses come from the injury status history
(``status_history.db``, read only: the morning digest owns recording it) and every relevant
goalie confirmation / alert counts as new. The summary is phone-sized (<= 1500 chars), e.g.
``Pre-game 5:02 PM: Bussi CONFIRMED (CAR vs FLA). Frost moved to F2 (was F1). No lineup changes
needed.`` and ``Pre-game: no changes. Lineup set.`` when nothing changed.
"""
from __future__ import annotations

import json
import logging
from datetime import date, datetime
from pathlib import Path
from typing import Any, Callable, Iterable, Literal, Mapping

from pydantic import BaseModel, Field

from .matching.normalize import normalize_name, normalize_team

log = logging.getLogger(__name__)

SNAPSHOT_DIR = "pregame"
SNAPSHOT_VERSION = 1
SUMMARY_LIMIT = 1500
MAX_ALERTS_STORED = 20
MAX_FA_GOALIES = 60               # free-agent goalies whose team plays today, best first
STREAMER_LIMIT = 3
FA_ALERT_LIMIT = 3
LINES_MAX_AGE = 6 * 3600.0
INJURED = ("dtd", "out", "ir", "ltir", "suspended")
UNAVAILABLE = ("out", "ir", "ltir", "suspended")
DTD_AVAILABILITY = 0.75
NO_CHANGES = "Pre-game: no changes. Lineup set."

ChangeKind = Literal["goalie_confirmed", "my_player_status_change", "lineup_change", "new_alert", "injury_new"]
# Loader: (league, settings, cache) -> (ctx, values, warnings)
Loader = Callable[[str, Any, Any], tuple[Any, Mapping[str, Any], list[str]]]


class Change(BaseModel):
    kind: ChangeKind
    text: str                        # one phone line, no trailing period
    scope: str | None = None         # goalie_confirmed: mine / opponent / streamer; new_alert: mine / fa
    player: str | None = None
    team: str | None = None
    priority: int = 5                # 1 = most urgent (summary order)
    short: str | None = None         # compact form for a merged phone line (opponent goalies)


class PregameResult(BaseModel):
    league: str
    day: date
    ran_at: datetime
    changes: list[Change] = Field(default_factory=list)
    lineup_recs: dict[str, Any] = Field(default_factory=dict)   # {"week": [Recommendation], "today": {...}}
    alerts: list[Any] = Field(default_factory=list)             # Recommendation (kind "alert")
    injuries: list[Any] = Field(default_factory=list)           # Recommendation (kind "injury")
    goalies_tonight: dict[str, dict[str, Any]] = Field(default_factory=dict)   # team -> named goalie
    summary_text: str = ""
    nothing_changed: bool = True
    baseline: str | None = None      # "daily" / "pregame" (source of the snapshot diffed against) / None
    baseline_at: str | None = None
    snapshot_path: str | None = None
    refresh: dict[str, Any] = Field(default_factory=dict)
    notify: list[str] | None = None  # webhook results when notify=True (None: not sent)
    warnings: list[str] = Field(default_factory=list)

    def notify_lines(self) -> list[str]:
        """Bullets for :func:`report.notify.notify_changes`."""
        if self.nothing_changed:
            return [self.summary_text]
        today = self.lineup_recs.get("today") or None
        return [c.text for c in display_changes(self.changes, today)] + [(today or {}).get("text") or ""]

    def title(self) -> str:
        return f"Pre-game {_clock(self.ran_at)} - {LEAGUE_LABEL.get(self.league, self.league)}"

    def to_dict(self) -> dict[str, Any]:
        """JSON-friendly summary (recommendations as titles)."""
        return {
            "league": self.league, "day": self.day.isoformat(), "ran_at": self.ran_at.isoformat(timespec="seconds"),
            "summary_text": self.summary_text, "nothing_changed": self.nothing_changed,
            "changes": [c.model_dump() for c in self.changes],
            "lineup": {"week": [r.title for r in self.lineup_recs.get("week") or []],
                       "today": self.lineup_recs.get("today") or {}},
            "alerts": [r.title for r in self.alerts], "injuries": [r.title for r in self.injuries],
            "goalies_tonight": self.goalies_tonight, "baseline": self.baseline, "baseline_at": self.baseline_at,
            "snapshot": self.snapshot_path, "refresh": self.refresh, "notify": self.notify,
            "warnings": self.warnings,
        }


LEAGUE_LABEL = {"espn": "ESPN", "fantrax": "Fantrax"}


# --------------------------------------------------------------------------- small helpers

def _clock(dt: datetime) -> str:
    """'5:02 PM' (no leading zero, portable across platforms)."""
    h = dt.hour % 12 or 12
    return f"{h}:{dt.minute:02d} {'AM' if dt.hour < 12 else 'PM'}"


def _short(e: BaseException, n: int = 160) -> str:
    s = str(e).strip().splitlines()[0] if str(e).strip() else type(e).__name__
    return s[:n]


def _last(name: str | None) -> str:
    """Last name for phone lines ('Pyotr Kochetkov' -> 'Kochetkov'; keeps 'van Riemsdyk')."""
    parts = (name or "").split()
    if len(parts) <= 1:
        return name or "?"
    tail = [parts[-1]]
    for w in reversed(parts[1:-1]):
        if w[:1].islower():
            tail.insert(0, w)
        else:
            break
    return " ".join(tail)


def _sentence(s: str) -> str:
    s = s.strip()
    return s if not s or s[-1] in ".!?" else s + "."


def snapshot_path(data_dir: Path | str, league: str, day: date) -> Path:
    return Path(data_dir) / SNAPSHOT_DIR / f"{league}-{day.isoformat()}.json"


def load_snapshot(data_dir: Path | str, league: str, day: date) -> dict | None:
    try:
        snap = json.loads(snapshot_path(data_dir, league, day).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    return snap if isinstance(snap, dict) and snap.get("version") == SNAPSHOT_VERSION else None


def save_snapshot(data_dir: Path | str, league: str, day: date, state: Mapping[str, Any]) -> Path:
    p = snapshot_path(data_dir, league, day)
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(state, indent=1, sort_keys=True, default=str), encoding="utf-8")
    tmp.replace(p)
    return p


# --------------------------------------------------------------------------- refresh

def refresh_sources(cache: Any, day: date, *, dfo_client: Any = None, injuries_fetch: Callable | None = None,
                    lines_max_age: float = LINES_MAX_AGE) -> tuple[list[Any], dict[str, Any]]:
    """Force-refresh the fast-moving sources; never raises. Returns (tonight's goalie starts, info).

    The goalie page and the injury feed bypass the cache (the fresh copies are stored, so the league
    load that follows reads them through its normal 3 h / 1 h TTLs); the line pages of the teams
    playing today are refetched only when older than ``lines_max_age``."""
    from .providers.dailyfaceoff import DailyFaceoffClient, make_refresh_fetch_text
    from .providers.injuries import INJURIES_URL, fetch_injuries

    info: dict[str, Any] = {"warnings": []}
    client = dfo_client
    if client is None and cache is not None:
        client = DailyFaceoffClient(fetch_text=make_refresh_fetch_text(cache, lines_max_age))
    starts: list[Any] = []
    if client is not None:
        try:
            starts = list(client.starting_goalies(day))
            info["goalies"] = f"{sum(1 for s in starts if s.is_confirmed)} confirmed / {len(starts)} listed"
        except Exception as e:  # noqa: BLE001 - one source must not sink the run
            info["warnings"].append(f"Daily Faceoff starting goalies refresh failed ({_short(e)})")
        teams = sorted({s.team for s in starts})
        if teams:
            try:
                lines = client.all_lines(teams)
                info["lines"] = f"{len(lines)}/{len(teams)} teams playing today (refetched if older than " \
                                f"{lines_max_age / 3600:.0f} h)"
                info["warnings"].extend(getattr(client, "warnings", None) or [])
            except Exception as e:  # noqa: BLE001
                info["warnings"].append(f"Daily Faceoff lines refresh failed ({_short(e)})")
    try:
        if injuries_fetch is not None:
            reports = fetch_injuries(injuries_fetch)
        elif cache is not None:
            reports = fetch_injuries(lambda url, params=None: cache.get_json(url, params=params, ttl=0))
        else:
            reports = None
        if reports is not None:
            info["injuries"] = f"{len(reports)} reports"
    except Exception as e:  # noqa: BLE001
        info["warnings"].append(f"injury feed refresh failed ({_short(e)}; {INJURIES_URL.split('/')[2]})")
    return starts, info


def _cached_starts(cache: Any, day: date, data_dir: Any, dfo_client: Any = None) -> list[Any]:
    """Tonight's goalie starts after the league load: the (fresh) cached page, else today's lines
    snapshot written by the load. Never raises."""
    from .providers.dailyfaceoff import DailyFaceoffClient, GoalieStart, make_cached_fetch_text
    from .providers.lines_enrich import LineSnapshotStore

    try:
        if dfo_client is not None:
            return list(dfo_client.starting_goalies(day))
        if cache is not None and hasattr(cache, "get_text"):
            return DailyFaceoffClient(fetch_text=make_cached_fetch_text(cache)).starting_goalies(day)
    except Exception as e:  # noqa: BLE001
        log.info("pregame: goalie page unavailable (%s); using today's lines snapshot", e)
    try:
        snap = LineSnapshotStore(data_dir).load(day) or {}
        return [GoalieStart.model_validate(s) for s in snap.get("starts") or []]
    except Exception:  # noqa: BLE001
        return []


# --------------------------------------------------------------------------- state

def goalies_state(starts: Iterable[Any], lines_snapshot: Mapping[str, Any] | None = None) -> dict[str, dict]:
    """{team: {"goalie", "strength", "game", "opponent", "home", "depth"}} for tonight: the goalie
    Daily Faceoff lists for each team (a confirmed report wins over an earlier one). ``depth`` is
    his G1 / G2 spot on the team's line page when known."""
    depth: dict[tuple[str, str], int] = {}
    for rec in ((lines_snapshot or {}).get("players") or {}).values():
        if rec.get("goalie_depth"):
            depth[(rec.get("team") or "", normalize_name(rec.get("name")))] = int(rec["goalie_depth"])
    out: dict[str, dict] = {}
    for s in starts:
        cur = out.get(s.team)
        if cur is not None and (cur["strength"] or "").lower() == "confirmed" and not s.is_confirmed:
            continue
        out[s.team] = {"goalie": s.goalie_name, "strength": s.strength, "game": s.game, "opponent": s.opponent,
                       "home": bool(s.home), "depth": depth.get((s.team, normalize_name(s.goalie_name)))}
    return out


def _plays_today(ctx: Any, p: Any) -> bool:
    from .recommend.alerts import plays_today
    try:
        return plays_today(ctx, p)
    except Exception:  # noqa: BLE001
        return True


def _start_prob(p: Any, pv: Any) -> float:
    from .recommend.lineup import confirmed_start_probability

    prob = confirmed_start_probability(p)
    if prob is not None:
        return prob
    share = getattr(pv, "start_share", None)
    return float(share) if share is not None else 0.5


def today_values(ctx: Any, values: Mapping[str, Any]) -> dict[str, Any]:
    """PlayerValues whose season value is TODAY's expected points: per-game value x (plays today)
    x availability x (goalies) start probability. Feed to ``optimal_lineup(..., "season")``."""
    out: dict[str, Any] = {}
    for p in ctx.my_team.players:
        pv = values.get(p.cid)
        if pv is None:
            continue
        v = float(pv.fpg or 0.0) if _plays_today(ctx, p) else 0.0
        if p.status in UNAVAILABLE:
            v = 0.0
        elif p.status == "dtd":
            v *= DTD_AVAILABILITY
        if p.is_goalie:
            v *= _start_prob(p, pv)
        out[p.cid] = pv.model_copy(update={"fpg_season": max(0.0, v)})
    return out


def today_lineup(ctx: Any, values: Mapping[str, Any]) -> dict[str, Any] | None:
    """Today's optimal lineup (daily-lineup leagues; None for a weekly lock): the players with a
    game today it starts / benches, and the moves from the current lineup."""
    from .recommend.lineup import _lock, optimal_lineup

    try:
        if _lock(ctx) == "weekly":
            return None
    except Exception:  # noqa: BLE001
        pass
    team = ctx.my_team
    tv = today_values(ctx, values)
    assign, total = optimal_lineup(team, tv, ctx.roster_shape, "season")
    chosen = set(assign.values())
    by = {p.cid: p for p in team.players}
    playing = {p.cid for p in team.players if _plays_today(ctx, p)}     # his team has a game today
    current = {s.player.cid for s in team.slots if s.player is not None and s.starting}
    start = sorted(c for c in chosen if c in playing and c in tv and tv[c].fpg_season > 0)
    bench = sorted(c for c in playing if c not in start)
    to_start = [c for c in start if c not in current]
    to_bench = sorted(c for c in current if c in playing and c not in chosen)
    idle_out = sorted(c for c in current if c not in playing and c not in chosen) if to_start else []
    cur_total = sum(tv[c].fpg_season for c in current if c in tv)
    names = lambda cs: [by[c].name for c in cs if c in by]  # noqa: E731
    lasts = lambda cs: ", ".join(_last(n) for n in names(cs))  # noqa: E731
    if to_start or to_bench:
        text = "Lineup: " + "; ".join(x for x in (f"start {lasts(to_start)}" if to_start else "",
                                                  f"bench {lasts(to_bench)}" if to_bench else "") if x)
        if idle_out:
            text += f"{'; also out' if to_bench else ' for'} {lasts(idle_out)} (no game)"
    else:
        text = "No lineup changes needed"
    return {"start": start, "bench": bench, "to_start": names(to_start), "to_bench": names(to_bench),
            "idle_out": names(idle_out),
            "optimal_total": round(total, 2), "current_total": round(cur_total, 2), "text": text,
            "set": not (to_start or to_bench)}


def _alert_entry(r: Any, mine_ids: set[str]) -> dict[str, Any]:
    p = r.subjects[0] if r.subjects else None
    codes = sorted({x.code for x in r.reasons})
    return {"title": r.title, "cid": p.cid if p else None, "name": p.name if p else None,
            "mine": bool(p and p.cid in mine_ids), "strength": r.strength, "codes": codes,
            "goalie_start": "CONFIRMED_START" in codes and not any(c in codes for c in ("LINE_CHANGE", "PP_UNIT"))}


def build_state(league: str, ctx: Any, values: Mapping[str, Any], starts: Iterable[Any] = (), *,
                alerts: list[Any] | None = None, source: str = "pregame", now: datetime | None = None,
                lines_snapshot: Mapping[str, Any] | None = None, lineup: dict | None = None) -> dict[str, Any]:
    """Compact JSON-able state of one league for the day (goalies tonight, my players' statuses /
    lines, today's optimal lineup, top alerts, free-agent goalies for streamer relevance)."""
    now = now or datetime.now()
    team = ctx.my_team
    mine = {p.cid: p for p in team.players}
    slot = {s.player.cid: s.slot for s in team.slots if s.player is not None}
    my_players = {}
    for cid, p in mine.items():
        my_players[cid] = {"name": p.name, "team": normalize_team(p.team), "goalie": p.is_goalie,
                           "status": p.status, "note": p.status_note, "line": p.line, "pp_unit": p.pp_unit,
                           "slot": slot.get(cid), "plays_today": _plays_today(ctx, p),
                           "start": p.confirmed_start}
    if alerts is None:
        from .recommend.alerts import recommend_line_alerts
        try:
            alerts = recommend_line_alerts(ctx, values)
        except Exception:  # noqa: BLE001
            alerts = []
    if lineup is None:
        try:
            lineup = today_lineup(ctx, values)
        except Exception:  # noqa: BLE001
            lineup = None
    fa_g = [p for p in ctx.free_agents if p.is_goalie and p.cid not in mine and _plays_today(ctx, p)]
    fa_g.sort(key=lambda p: -float(getattr(values.get(p.cid), "fpg", 0.0) or 0.0))
    return {
        "version": SNAPSHOT_VERSION, "league": league, "date": ctx.as_of.isoformat(), "source": source,
        "taken_at": now.isoformat(timespec="seconds"),
        "goalies": goalies_state(starts, lines_snapshot),
        "my_players": my_players,
        "lineup_today": ({k: lineup[k] for k in ("start", "bench", "set", "text")} if lineup else None),
        "alerts": [_alert_entry(r, set(mine)) for r in (alerts or [])[:MAX_ALERTS_STORED]],
        "fa_goalies": [{"name": p.name, "team": normalize_team(p.team)} for p in fa_g[:MAX_FA_GOALIES]],
    }


# --------------------------------------------------------------------------- diff

def _is_conf(g: Mapping[str, Any] | None) -> bool:
    return bool(g) and (g.get("strength") or "").strip().lower() == "confirmed"


def _is_named(g: Mapping[str, Any] | None) -> bool:
    return bool(g) and (g.get("strength") or "").strip().lower() in ("confirmed", "likely", "expected")


def _goalie_word(g: Mapping[str, Any]) -> str:
    s = (g.get("strength") or "").strip().lower()
    return "CONFIRMED" if s == "confirmed" else (s or "listed")


def diff_goalies(prev: Mapping[str, Any] | None, cur: Mapping[str, Any]) -> list[Change]:
    pg = (prev or {}).get("goalies") or {}
    my = cur.get("my_players") or {}
    my_goalie_teams: dict[str, list[str]] = {}
    skater_teams: set[str] = set()
    for rec in my.values():
        if not rec.get("team"):
            continue
        if rec.get("goalie"):
            my_goalie_teams.setdefault(rec["team"], []).append(rec["name"])
        elif rec.get("plays_today") and rec.get("status") not in UNAVAILABLE:
            skater_teams.add(rec["team"])
    fa = {normalize_name(g["name"]): g for g in cur.get("fa_goalies") or []}
    fa_rank = {normalize_name(g["name"]): i for i, g in enumerate(cur.get("fa_goalies") or [])}
    out: list[Change] = []
    streamers: list[tuple[int, Change]] = []
    for team, g in sorted((cur.get("goalies") or {}).items()):
        p = pg.get(team)
        switched = p is not None and normalize_name(p.get("goalie")) != normalize_name(g.get("goalie"))
        newly_confirmed = _is_conf(g) and not (p is not None and _is_conf(p) and not switched)
        if not (newly_confirmed or (switched and _is_named(g))):
            continue
        name, opp = g.get("goalie") or "?", g.get("opponent") or "?"
        word = _goalie_word(g)
        was = f"; was {_last(p.get('goalie'))}" if switched else ""
        if team in my_goalie_teams:
            mine_names = my_goalie_teams[team]
            if any(normalize_name(n) == normalize_name(name) for n in mine_names):
                out.append(Change(kind="goalie_confirmed", scope="mine", player=name, team=team, priority=1,
                                  text=f"{_last(name)} {word} ({team} vs {opp}{was})"))
            else:
                sits = ", ".join(_last(n) for n in mine_names)
                out.append(Change(kind="goalie_confirmed", scope="mine", player=mine_names[0], team=team, priority=1,
                                  text=f"{sits} sits: {_last(name)} {word} for {team} vs {opp}"))
            continue
        if opp in skater_teams:
            backup = g.get("depth") == 2
            short = f"{_last(name)} ({team}{', backup' if backup else ''})" if _is_conf(g) and not switched else None
            out.append(Change(kind="goalie_confirmed", scope="opponent", player=name, team=team, priority=4,
                              short=short, text=f"Your {opp} skaters face {_last(name)}"
                                                f"{' (backup)' if backup else ''}, {word} for {team}{was}"))
            continue
        key = normalize_name(name)
        if key in fa and _is_conf(g):
            streamers.append((fa_rank.get(key, 99), Change(
                kind="goalie_confirmed", scope="streamer", player=name, team=team, priority=6,
                text=f"Streamer: {_last(name)} (free agent) CONFIRMED ({team} vs {opp})")))
    out.extend(c for _, c in sorted(streamers, key=lambda x: x[0])[:STREAMER_LIMIT])
    return out


def diff_players(prev: Mapping[str, Any] | None, cur: Mapping[str, Any],
                 baseline_status: Mapping[str, str] | None = None) -> list[Change]:
    """Status changes (``injury_new`` / ``my_player_status_change``) and line / PP moves
    (``new_alert``, scope mine) of my players."""
    pp = (prev or {}).get("my_players") or {}
    out: list[Change] = []
    for cid, c in (cur.get("my_players") or {}).items():
        p = pp.get(cid)
        old = p.get("status") if p is not None else (baseline_status or {}).get(cid)
        new = c.get("status")
        name, team = c.get("name"), c.get("team")
        if old is not None and new != old and not (old in ("healthy", "unknown") and new in ("healthy", "unknown")):
            note = f" ({c['note']})" if c.get("note") else ""
            if new in INJURED and old not in INJURED:
                tail = ": bench him" if c.get("plays_today") and c.get("slot") not in ("BN", "IR", None) \
                    and new in UNAVAILABLE else ""
                out.append(Change(kind="injury_new", player=name, team=team, priority=2,
                                  text=f"{_last(name)} now {new.upper()}{note}{tail}"))
            elif new not in INJURED:
                out.append(Change(kind="my_player_status_change", player=name, team=team, priority=2,
                                  text=f"{_last(name)} cleared: {old.upper()} -> {new}"))
            else:
                out.append(Change(kind="my_player_status_change", player=name, team=team, priority=2,
                                  text=f"{_last(name)} now {new.upper()} (was {old.upper()}){note}"))
        if p is None or c.get("goalie"):
            continue
        a, b = p.get("line"), c.get("line")
        moved: list[str] = []
        if a and b and a != b:
            moved.append(f"moved to {b.upper()} (was {a.upper()})")
        if a and b and (p.get("pp_unit") or None) != (c.get("pp_unit") or None):
            before, after = (p.get("pp_unit") or "no PP").upper(), (c.get("pp_unit") or "no PP").upper()
            moved.append(f"{before} -> {after}".replace("NO PP", "no PP"))
        if moved:
            out.append(Change(kind="new_alert", scope="mine", player=name, team=team, priority=3,
                              text=f"{_last(name)} " + ", ".join(moved)))
    return out


def diff_lineup(prev: Mapping[str, Any] | None, cur: Mapping[str, Any]) -> list[Change]:
    pl, cl = (prev or {}).get("lineup_today"), cur.get("lineup_today")
    if not pl or not cl:
        return []
    names = {cid: rec.get("name") for cid, rec in (cur.get("my_players") or {}).items()}
    before, after = set(pl.get("start") or []), set(cl.get("start") or [])
    playing_before = before | set(pl.get("bench") or [])
    out: list[Change] = []
    for cid in sorted(after - before):
        if cid in names:
            was = "sat this morning" if cid in playing_before else "not in the morning lineup"
            out.append(Change(kind="lineup_change", player=names[cid], priority=3,
                              text=f"Start {_last(names[cid])} ({was})"))
    for cid in sorted(before - after):
        if cid in names:
            out.append(Change(kind="lineup_change", player=names[cid], priority=3,
                              text=f"Sit {_last(names[cid])} (was starting this morning)"))
    return out


def diff_alerts(prev: Mapping[str, Any] | None, cur: Mapping[str, Any], skip_players: set[str] = frozenset()
                ) -> list[Change]:
    """Line alerts whose title is new since ``prev`` (goalie-start alerts are covered by
    :func:`diff_goalies`; my players already reported by a line diff are skipped)."""
    seen = {a.get("title") for a in (prev or {}).get("alerts") or []}
    out: list[Change] = []
    fa = 0
    for a in cur.get("alerts") or []:
        if a.get("title") in seen or a.get("goalie_start") or a.get("name") in skip_players:
            continue
        if not a.get("mine"):
            if fa >= FA_ALERT_LIMIT:
                continue
            fa += 1
        out.append(Change(kind="new_alert", scope="mine" if a.get("mine") else "fa", player=a.get("name"),
                          priority=3 if a.get("mine") else 6, text=str(a.get("title") or "")))
    return out


def diff_states(prev: Mapping[str, Any] | None, cur: Mapping[str, Any],
                baseline_status: Mapping[str, str] | None = None) -> list[Change]:
    """Everything that changed between two states of the same league and day, most urgent first."""
    changes = diff_goalies(prev, cur)
    players = diff_players(prev, cur, baseline_status)
    changes += players
    changes += diff_lineup(prev, cur)
    moved = {c.player for c in players if c.kind == "new_alert" and c.player}
    changes += diff_alerts(prev, cur, moved)
    return sorted(changes, key=lambda c: c.priority)


# --------------------------------------------------------------------------- summary

def display_changes(changes: list[Change], lineup: Mapping[str, Any] | None) -> list[Change]:
    """Changes worth a line on the phone: ``lineup_change`` entries are dropped when today's lineup
    needs moves anyway (the "Lineup: start X; bench Y" tail says the same, as the action to take),
    and the confirmed goalies my skaters face collapse into one line
    ("Confirmed vs your skaters: Swayman (BOS), Dobes (MTL, backup)")."""
    out = [c for c in changes if not (lineup and not lineup.get("set") and c.kind == "lineup_change")]
    opp = [c for c in out if c.kind == "goalie_confirmed" and c.scope == "opponent" and c.short]
    if len(opp) < 2:
        return out
    merged = Change(kind="goalie_confirmed", scope="opponent", priority=opp[0].priority,
                    text="Confirmed vs your skaters: " + ", ".join(c.short for c in opp))
    first = out.index(opp[0])
    rest = [c for c in out if c not in opp]
    return rest[:first] + [merged] + rest[first:]


def summarize(changes: list[Change], lineup: Mapping[str, Any] | None, ran_at: datetime,
              limit: int = SUMMARY_LIMIT) -> str:
    """Phone-sized summary (<= ``limit`` chars)."""
    tail = (lineup or {}).get("text") if lineup else None
    if changes:
        changes = display_changes(changes, lineup)
        if not changes and tail:          # only start / sit changes: the lineup tail says it all
            return _sentence(f"Pre-game {_clock(ran_at)}: {tail}")[:limit]
    if not changes:
        if lineup is None or lineup.get("set"):
            return NO_CHANGES
        return _sentence(f"Pre-game: no changes. {tail}")[:limit]
    head = f"Pre-game {_clock(ran_at)}:"
    tail_s = f" {_sentence(tail)}" if tail else ""
    body: list[str] = []
    for i, c in enumerate(changes):
        more = len(changes) - i - 1
        piece = f" {_sentence(c.text)}"
        reserve = len(tail_s) + (len(f" (+{more} more)") if more else 0)
        if len(head) + sum(map(len, body)) + len(piece) + reserve > limit:
            body.append(f" (+{len(changes) - i} more)")
            break
        body.append(piece)
    text = head + "".join(body) + tail_s
    return text if len(text) <= limit else text[: limit - 3].rstrip() + "..."


# --------------------------------------------------------------------------- failures

def failure_message(league: str, error: BaseException | str) -> str:
    """Phone line for a league that did not load (login expired, provider down, ...)."""
    msg = str(error)
    low = msg.lower()
    if league == "fantrax" and any(k in low for k in ("not logged in", "login", "cookie", "session")):
        return "Fantrax login expired: refresh FANTRAX_COOKIE (or run `fm auth fantrax --login`)"
    if league == "espn" and any(k in low for k in ("denied access", "espn_s2", "swid")):
        return "ESPN login expired: refresh ESPN_S2 / ESPN_SWID in .env"
    first = msg.strip().splitlines()[0][:160] if msg.strip() else type(error).__name__
    return f"{LEAGUE_LABEL.get(league, league)} not loaded: {first}"


def has_webhooks(settings: Any) -> bool:
    return bool(getattr(settings, "discord_webhook_url", None) or getattr(settings, "slack_webhook_url", None))


# --------------------------------------------------------------------------- main entry

def default_loader(league: str, settings: Any, cache: Any) -> tuple[Any, Mapping[str, Any], list[str]]:
    from .cli_backtest import _load_league_full

    ctx, values, _, warnings, _ = _load_league_full(league, settings, cache, False)
    return ctx, values, warnings


def run_pregame(league: str, settings: Any, cache: Any, as_of: date | None = None, notify: bool = False,
                force_refresh: bool = True, *, quiet_if_unchanged: bool = False, loader: Loader | None = None,
                dfo_client: Any = None, injuries_fetch: Callable | None = None, now: datetime | None = None,
                notifier: Callable[..., list[str]] | None = None, write_snapshot: bool = True) -> PregameResult:
    """One league's pre-game run (see the module docstring). Raises ``ProviderError`` (or whatever
    the loader raises) when the league cannot be loaded; every later step degrades to a warning.

    ``notify``: post the changes (or the no-change line) with ``report.notify.notify_changes``;
    with ``quiet_if_unchanged`` nothing is sent when nothing changed. ``loader`` / ``dfo_client`` /
    ``injuries_fetch`` / ``notifier`` are injectable for tests."""
    from .providers.lines_enrich import LineSnapshotStore
    from .recommend.alerts import recommend_line_alerts
    from .recommend.injuries import recommend_injuries
    from .recommend.lineup import recommend_lineup

    now = now or datetime.now()
    day = as_of or now.date()
    data_dir = getattr(settings, "fm_data_dir", ".")
    warnings: list[str] = []
    refresh: dict[str, Any] = {}
    starts: list[Any] | None = None
    if force_refresh:
        starts, refresh = refresh_sources(cache, day, dfo_client=dfo_client, injuries_fetch=injuries_fetch)
        warnings.extend(refresh.get("warnings") or [])
        starts = starts or None
    ctx, values, load_warnings = (loader or default_loader)(league, settings, cache)
    warnings.extend(str(w) for w in load_warnings or [])
    if as_of is not None and ctx.as_of != as_of:
        ctx.as_of = as_of
    if starts is None:
        starts = _cached_starts(cache, day, data_dir, dfo_client)

    def safe(label: str, fn: Callable[[], Any], default: Any) -> Any:
        try:
            return fn()
        except Exception as e:  # noqa: BLE001 - one step must not sink the run
            warnings.append(f"{label} failed ({_short(e)})")
            return default

    week = safe("week lineup", lambda: recommend_lineup(ctx, values, "week"), [])
    lineup = safe("today's lineup", lambda: today_lineup(ctx, values), None)
    alerts = safe("line alerts", lambda: recommend_line_alerts(ctx, values), [])
    mine_ids = safe("roster", lambda: [p.cid for p in ctx.my_team.players], [])
    history = safe("injury status history", lambda: _history_snapshots(data_dir), {})
    baseline_status = {c: history[c].status for c in mine_ids if c in history}
    injuries = safe("injury alerts", lambda: recommend_injuries(ctx, values, history), [])
    lines_snap = safe("lines snapshot", lambda: LineSnapshotStore(data_dir).load(day), None)
    state = build_state(league, ctx, values, starts, alerts=alerts, source="pregame", now=now,
                        lines_snapshot=lines_snap, lineup=lineup)
    state["date"] = day.isoformat()
    prev = load_snapshot(data_dir, league, day)
    changes = diff_states(prev, state, baseline_status if prev is None else None)
    res = PregameResult(league=league, day=day, ran_at=now, changes=changes,
                        lineup_recs={"week": week, "today": lineup or {}}, alerts=alerts, injuries=injuries,
                        goalies_tonight=state["goalies"], nothing_changed=not changes,
                        baseline=(prev or {}).get("source"), baseline_at=(prev or {}).get("taken_at"),
                        refresh={k: v for k, v in refresh.items() if k != "warnings"}, warnings=warnings)
    res.summary_text = summarize(changes, lineup, now)
    if write_snapshot:
        try:
            res.snapshot_path = str(save_snapshot(data_dir, league, day, state))
        except OSError as e:
            warnings.append(f"pre-game snapshot not written ({_short(e)})")
    # An unset lineup at 5 PM is the most useful thing to send: post even when nothing changed.
    _today = res.lineup_recs.get("today") or {}
    _pending_today = bool(_today) and not _today.get("set", True)  # today's lineup still needs moves
    if notify and not (quiet_if_unchanged and res.nothing_changed and not _pending_today):
        if notifier is None:
            from .report.notify import notify_changes as notifier
        res.notify = notifier(settings, res.title(), res.notify_lines())
    return res


def _history_snapshots(data_dir: Any) -> dict:
    """Latest injury status per player (``status_history.db``), read only."""
    from .recommend.injuries import StatusHistory

    hist = StatusHistory(data_dir)
    try:
        return hist.last()
    finally:
        hist.close()


def write_daily_snapshot(league: str, settings: Any, ctx: Any, values: Mapping[str, Any],
                         now: datetime | None = None) -> Path:
    """The morning baseline: called by ``fm harness daily`` after each league load (goalie starts
    from today's Daily Faceoff lines snapshot, which the load just wrote)."""
    from .providers.dailyfaceoff import GoalieStart
    from .providers.lines_enrich import LineSnapshotStore

    data_dir = getattr(settings, "fm_data_dir", ".")
    snap = LineSnapshotStore(data_dir).load(ctx.as_of) or {}
    starts = []
    for s in snap.get("starts") or []:
        try:
            starts.append(GoalieStart.model_validate(s))
        except Exception:  # noqa: BLE001
            continue
    state = build_state(league, ctx, values, starts, source="daily", now=now, lines_snapshot=snap)
    return save_snapshot(data_dir, league, ctx.as_of, state)


__all__ = ["Change", "PregameResult", "run_pregame", "refresh_sources", "build_state", "diff_states",
           "summarize", "failure_message", "write_daily_snapshot", "load_snapshot", "save_snapshot",
           "snapshot_path", "today_lineup", "NO_CHANGES"]
