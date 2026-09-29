"""Enrich a provider's LeagueContext with NHL data.

Steps (each one is best-effort: a failing source adds a warning to ``ctx.warnings`` and the
rest still runs, so offline / partial runs degrade gracefully):

1. schedule: regular-season dates per NHL team, games per day, opening night
2. NHL season rows (current, prior and the two seasons before it) and current rosters (roster
   goalies kept in ``ctx.nhl_goalies`` for teammate-aware goalie start shares)
3. crosswalk: provider player -> NHL id (``player.ids["nhl"]``)
4. birth dates (and missing NHL teams) from rosters
5. injuries: ESPN injury feed merged with provider status (more severe wins)
6. missing ``season`` / ``prior`` / ``prior2`` / ``prior3`` stat lines built from NHL season rows
   (prior2 / prior3 feed the 3-season baseline; those finished seasons are cached 30 days)
6b. pedigree: draft position and career GP from NHL player pages for young (<= 24) or
   unproven (no prior season with >= 40 GP) players, rostered first, then the most-owned
   free agents; at most ``pedigree_limit`` uncached pages per run (30-day cache)
6c. ``rookies=True`` (needs the HTTP cache): rookie evidence (``providers.rookie_enrich``): league history (every league's
   seasonTotals, for the NHLe prior) of unproven skaters - 6b's landing pages reused, then at most
   200 uncached pages (30-day cache) - and news role signals (``providers.news_roles``: RotoWire /
   ESPN through the 30-minute RSS cache; optional free LLM pass when OPENROUTER_API_KEY is set,
   cached in ``<fm_data_dir>/news_roles.json``; FM_NEWS_ROLES_LLM=0 turns it off)
7. ``deep=True``: last7/15/30 lines from per-player game logs (date windows ending ``as_of``)
7b. ``preseason=True``: preseason (gameType 1) lines of unproven skaters from NHL box scores
   (``providers.preseason_enrich``; needs the HTTP cache; until 45 days after opening night)
8. ``deployment=True``: TOI / PP share per skater (``providers.deployment_enrich``) from the
   harness ledger (``<fm_data_dir>/harness.db``, only when it exists; never created here), else
   the NHL season reports (preseason: last season's); goalie starts / back-to-back rates from the
   ledger once a team has 5 games there (``ctx.goalie_actual_starts`` / ``ctx.b2b_second_night``)
9. ``lines=True``: Daily Faceoff lines, PP units and starting goalies (``providers.lines_enrich``;
   32 team pages cached 12 h, the starting-goalies page 3 h, one snapshot per day under
   ``<fm_data_dir>/lines``)
10. ``xg=True``: MoneyPuck expected goals (``providers.xg_enrich``; 2 season CSVs, 12 h / 30 d)

Every run also sets ``ctx.sources`` (``collect_sources``): one structured ``SourceStatus`` per
source (league provider from its warnings, tracked HTTP sources, failed steps, Daily Faceoff,
MoneyPuck, deployment ledger, preseason, odds archive) for ``report.health``; the free-text
``source_notes`` / ``warnings`` are unchanged.

Steps 9 and 10 need an HTTP cache (skipped when ``cache`` is None unless a client is injected),
so tests and offline runs never reach those sites by accident.
"""
from __future__ import annotations

import re
import time
from datetime import date, datetime, timedelta, timezone
from typing import Any, Callable, Iterable

from ..cache import cache_key
from ..matching.crosswalk import Crosswalk, nhl_candidates
from ..matching.matcher import Candidate, PlayerIndex
from ..matching.normalize import normalize_team
from ..models import CANONICAL_STATS, LeagueContext, Player, SourceStatus, StatLine
from .injuries import INJURIES_URL, InjuryReport, fetch_injuries
from .nhl import CachedNhlFetch
from .nhl import (NHL_TEAMS, WEB_BASE, NhlClient, NhlGameLogEntry, NhlGoalieSeason, NhlRosterPlayer, NhlSkaterSeason,
                  current_season, games_per_day, prior_season)

HOUR = 3600.0
TTL_NHL_STATS = 6 * HOUR
TTL_NHL_HISTORY = 30 * 24 * HOUR      # finished seasons before the prior one
TTL_GAME_LOG = 12 * HOUR
TTL_SCHEDULE = 7 * 24 * HOUR
TTL_ROSTER = 30 * 24 * HOUR
TTL_LANDING = 30 * 24 * HOUR
TTL_INJURIES = 1 * HOUR
TTL_RSS = 0.5 * HOUR

DEEP_FA_LIMIT = 40
# Pedigree (draft position, career GP) from NHL player pages: young or unproven players only.
PEDIGREE_LIMIT = 150
PEDIGREE_MAX_AGE = 24.0
PEDIGREE_PROVEN_GP = 40
PEDIGREE_FA_LIMIT = 150

# More severe wins when merging provider and feed status; "suspended" is handled separately.
SEVERITY = {"unknown": 0, "healthy": 0, "dtd": 1, "out": 2, "ir": 3, "ltir": 4}


# --------------------------------------------------------------------------- cached fetchers

_SEASON_RE = re.compile(r"seasonId=(\d{8})")


def _stats_season(params: dict | None) -> int | None:
    m = _SEASON_RE.search(str((params or {}).get("cayenneExp", "")))
    return int(m.group(1)) if m else None


def ttl_for(url: str, params: dict | None = None, today: date | None = None) -> tuple[str, float]:
    """(source label, TTL seconds) for a URL. NHL stats reports for seasons before the prior
    one never change, so they are cached for 30 days."""
    if url.startswith(INJURIES_URL):
        return "injuries", TTL_INJURIES
    if "rss" in url:
        return "news", TTL_RSS
    if "/game-log/" in url:
        return "NHL game logs", TTL_GAME_LOG
    if "/club-schedule-season/" in url or "/schedule/" in url:
        return "NHL schedule", TTL_SCHEDULE
    if "/roster/" in url:
        return "NHL rosters", TTL_ROSTER
    if url.endswith("/landing"):
        return "NHL player pages", TTL_LANDING
    if "api.nhle.com/stats" in url:
        s = _stats_season(params)
        if s is not None and s < prior_season(current_season(today)):
            return "NHL stats (history)", TTL_NHL_HISTORY
        return "NHL stats", TTL_NHL_STATS
    return "NHL", TTL_NHL_STATS


# Display names of the tracked sources (SourceStatus.name) and the sources valuation reads.
SOURCE_NAMES = {"NHL stats": "NHL stats", "NHL stats (history)": "NHL stats (past seasons)",
                "NHL schedule": "NHL schedules", "NHL rosters": "NHL rosters", "NHL player pages": "NHL player pages",
                "NHL game logs": "NHL game logs", "injuries": "Injuries", "news": "News feeds", "NHL": "NHL (other)"}
VALUATION_SOURCES = frozenset({"NHL stats", "NHL schedules", "NHL rosters", "Injuries", "Daily Faceoff lines",
                               "Daily Faceoff goalies", "MoneyPuck", "Deployment ledger", "Fantrax", "ESPN"})


def _utc(epoch: float | None) -> datetime | None:
    return None if epoch is None else datetime.fromtimestamp(float(epoch), tz=timezone.utc)


def _age(seconds: float) -> str:
    if seconds < 90:
        return "just now"
    if seconds < 90 * 60:
        return f"{seconds / 60:.0f}m ago"
    if seconds < 36 * HOUR:
        return f"{seconds / HOUR:.0f}h ago"
    return f"{seconds / (24 * HOUR):.0f}d ago"


class SourceTracker:
    """Routes fetches through HttpCache with per-source TTLs and records data freshness."""

    def __init__(self, cache: Any):
        self.cache = cache
        # label -> {"requests": successful fetches, "oldest": epoch of the oldest data served,
        # "ttl", "failed": failed fetches, "error": last failure, "last_good": newest cached copy
        # of a failed request (epoch; the cache keeps expired rows)}
        self.sources: dict[str, dict[str, Any]] = {}

    def _entry(self, label: str, ttl: float | None = None) -> dict[str, Any]:
        s = self.sources.setdefault(label, {"requests": 0, "oldest": None, "ttl": ttl, "failed": 0,
                                            "error": None, "last_good": None})
        if ttl is not None:
            s["ttl"] = ttl
        return s

    def _fetched_at(self, url: str, params: dict | None) -> float | None:
        lookup = getattr(self.cache, "_lookup", None)
        if lookup is None:
            return None
        try:
            row = lookup(cache_key("GET", url, params, None))
        except Exception:
            return None
        return float(row[3]) if row else None

    def _record(self, url: str, params: dict | None, label: str, ttl: float | None = None) -> None:
        s = self._entry(label, ttl)
        s["requests"] += 1
        fetched = self._fetched_at(url, params)
        if fetched is not None:
            s["oldest"] = fetched if s["oldest"] is None else min(s["oldest"], fetched)

    def _record_failure(self, url: str, params: dict | None, label: str, ttl: float, e: Exception) -> None:
        s = self._entry(label, ttl)
        s["failed"] += 1
        s["error"] = _short(e)
        fetched = self._fetched_at(url, params)
        if fetched is not None:
            s["last_good"] = fetched if s["last_good"] is None else max(s["last_good"], fetched)

    def fetch_json(self, url: str, params: dict | None = None) -> Any:
        label, ttl = ttl_for(url, params)
        try:
            data = self.cache.get_json(url, params=params, ttl=ttl)
        except Exception as e:
            self._record_failure(url, params, label, ttl, e)
            raise
        self._record(url, params, label, ttl)
        return data

    def fetch_text(self, url: str, params: dict | None = None) -> str:
        label, ttl = ttl_for(url, params)
        try:
            text = self.cache.get_text(url, params=params, ttl=ttl)
        except Exception as e:
            self._record_failure(url, params, label, ttl, e)
            raise
        self._record(url, params, label, ttl)
        return text

    def statuses(self, now: float | None = None) -> list[SourceStatus]:
        """One SourceStatus per tracked source: fail when every request failed, warn when some
        did; ``fetched_at`` is the oldest data served (or the newest cached copy of a failure)."""
        now = now or time.time()
        out = []
        for label, s in sorted(self.sources.items()):
            name = SOURCE_NAMES.get(label, label)
            good, bad = int(s["requests"]), int(s.get("failed") or 0)
            t = s["oldest"] if good else s.get("last_good")
            if not bad:
                severity, detail = "ok", None
            elif good:
                severity, detail = "warn", f"{bad} of {good + bad} requests failed ({s['error']})"
            else:
                severity, detail = "fail", f"unavailable ({s['error']})"
            out.append(SourceStatus(name=name, ok=severity != "fail", severity=severity,  # type: ignore[arg-type]
                                    fetched_at=_utc(t), age_seconds=None if t is None else max(0.0, now - t),
                                    ttl_seconds=s.get("ttl"), detail=detail, requests=good + bad,
                                    feeds_valuation=name in VALUATION_SOURCES))
        return out

    def notes(self, now: float | None = None) -> list[str]:
        now = now or time.time()
        out = []
        for label, s in sorted(self.sources.items()):
            fresh = f", data {_age(now - s['oldest'])}" if s["oldest"] else ""
            out.append(f"{label}: {s['requests']} requests{fresh}")
        return out


def make_fetch_json(cache: Any) -> Callable[[str, dict | None], Any]:
    return SourceTracker(cache).fetch_json


def make_fetch_text(cache: Any) -> Callable[[str, dict | None], str]:
    return SourceTracker(cache).fetch_text


# --------------------------------------------------------------------------- pure helpers

def merge_status(provider: str, feed: str) -> str:
    """More severe of the two (healthy < dtd < out < ir < ltir); suspension is kept unless
    the other source reports an injury at least as serious as ``out``."""
    suspended = "suspended" in (provider, feed)
    a = "healthy" if provider == "suspended" else provider
    b = "healthy" if feed == "suspended" else feed
    worst = b if SEVERITY.get(b, 0) > SEVERITY.get(a, 0) else a
    if suspended and SEVERITY.get(worst, 0) < SEVERITY["out"]:
        return "suspended"
    return worst


def _injury_note(r: InjuryReport) -> str | None:
    parts = []
    if r.injury_type and (not r.comment or r.injury_type.lower() not in r.comment.lower()):
        parts.append(f"{r.injury_type}:")
    if r.comment:
        parts.append(r.comment)
    if r.return_date:
        parts.append(f"(est. return {r.return_date.isoformat()})")
    note = " ".join(parts).strip()
    if len(note) > 220:
        note = note[:217].rstrip() + "..."
    return note or None


def _pos_str(p: Player) -> str | None:
    pos = [x for x in p.positions if x != "F"] or p.positions
    return "/".join(pos) if pos else None


def _teams_agree(a: str | None, b: str | None) -> bool:
    ta, tb = normalize_team(a), normalize_team(b)
    return not ta or not tb or ta == tb


def merge_injuries(players: Iterable[Player], reports: list[InjuryReport]) -> int:
    """Apply injury reports to players (ESPN id first, else name + team). Returns matches."""
    if not reports:
        return 0
    by_espn = {str(r.espn_player_id): r for r in reports if r.espn_player_id is not None}
    index = PlayerIndex(Candidate(key=i, name=r.player_name, team=r.team, position=r.position)
                        for i, r in enumerate(reports))
    applied = 0
    for p in players:
        rep = by_espn.get(p.ids.get("espn", "")) if p.cid.startswith("espn:") else None
        if rep is None:
            m = index.match(p.name, team=p.team, position=_pos_str(p))
            if not m.matched:
                continue
            cand = reports[m.key]
            if not _teams_agree(p.team, cand.team):
                continue
            rep = cand
        applied += 1
        p.status = merge_status(p.status, rep.status)  # type: ignore[assignment]
        note = _injury_note(rep)
        if note:
            p.status_note = note
    return applied


def statline_from_nhl(row: NhlSkaterSeason | NhlGoalieSeason, split: str) -> StatLine | None:
    """Canonical StatLine from an NHL season row (None if no games)."""
    stats = {k: float(v) for k, v in row.stats.items() if k in CANONICAL_STATS}
    if "PPP" in stats and "PPG" in stats:
        stats.setdefault("PPA", stats["PPP"] - stats["PPG"])
    if "SHP" in stats and "SHG" in stats:
        stats.setdefault("SHA", stats["SHP"] - stats["SHG"])
    gp = int(stats.get("GP", 0))
    if gp <= 0:
        return None
    return StatLine(split=split, gp=gp, stats=stats)


def fill_stat_lines(players: Iterable[Player], current: dict[int, Any], prior: dict[int, Any],
                    prior2: dict[int, Any] | None = None, prior3: dict[int, Any] | None = None) -> dict[str, int]:
    """Add missing ``season`` / ``prior`` / ``prior2`` / ``prior3`` lines (seasons N, N-1, N-2,
    N-3) from NHL rows keyed by NHL id."""
    sources = (("season", current), ("prior", prior), ("prior2", prior2 or {}), ("prior3", prior3 or {}))
    filled = {split: 0 for split, _ in sources}
    for p in players:
        nid = p.nhl_id
        if nid is None:
            continue
        for split, rows in sources:
            if split in p.lines or nid not in rows:
                continue
            line = statline_from_nhl(rows[nid], split)
            if line is not None:
                p.lines[split] = line
                filled[split] += 1
    return filled


def recent_lines(entries: list[NhlGameLogEntry], as_of: date,
                 windows: tuple[tuple[str, int], ...] = (("last7", 7), ("last15", 15), ("last30", 30))
                 ) -> dict[str, StatLine]:
    """Aggregate game-log entries into date-window StatLines ending at ``as_of`` (inclusive)."""
    out: dict[str, StatLine] = {}
    for split, days in windows:
        sel = [e for e in entries if as_of - timedelta(days=days) < e.date <= as_of]
        if not sel:
            continue
        tot: dict[str, float] = {}
        toi = 0.0
        for e in sel:
            toi += e.toi_seconds or 0.0
            for k, v in e.stats.items():
                if k in CANONICAL_STATS and k not in ("SVPCT", "GAA"):
                    tot[k] = tot.get(k, 0.0) + float(v)
        tot["GP"] = float(len(sel))
        if any(e.is_goalie for e in sel):
            sa, ga = tot.get("SA", 0.0), tot.get("GA", 0.0)
            tot["SVPCT"] = (sa - ga) / sa if sa else 0.0
            tot["GAA"] = ga * 3600.0 / toi if toi else ga / len(sel)
        out[split] = StatLine(split=split, gp=len(sel), stats=tot)
    return out


# --------------------------------------------------------------------------- orchestration

def _short(e: Exception) -> str:
    msg = str(e) or e.__class__.__name__
    return f"{e.__class__.__name__}: {msg[:140]}"


def enrich_context(ctx: LeagueContext, settings: Any, cache: Any, deep: bool = False, *,
                   nhl: NhlClient | None = None, injuries_fetch: Callable | None = None,
                   crosswalk: Crosswalk | None = None, teams: Iterable[str] = NHL_TEAMS,
                   deep_fa_limit: int = DEEP_FA_LIMIT, pedigree_limit: int = PEDIGREE_LIMIT,
                   deployment: bool = True, lines: bool = True, xg: bool = True, preseason: bool = True,
                   ledger: Any = None, lines_client: Any = None, xg_client: Any = None,
                   rookies: bool = True) -> LeagueContext:
    """Mutates and returns ``ctx``; never raises for a single failing data source.

    ``deployment`` / ``lines`` / ``xg`` switch steps 8-10 off (tests, offline runs). ``ledger``
    (a harness Ledger) overrides opening ``<fm_data_dir>/harness.db``; ``lines_client`` (a
    DailyFaceoffClient) and ``xg_client`` (a MoneyPuckClient) replace the cached network clients."""
    tracker = SourceTracker(cache) if cache is not None else None
    fetch = tracker.fetch_json if tracker else None
    season = current_season(ctx.as_of)
    prior = prior_season(season)
    prior2 = prior_season(prior)
    prior3 = prior_season(prior2)
    client = nhl or NhlClient(fetch_json=fetch, season=season)
    teams = list(teams)
    own_xw = crosswalk is None
    players = ctx.all_players()
    # structured state for ctx.sources: source name -> {"attempts", "failed", "detail"} per
    # best-effort step, and the results of steps 7b-10
    failures: dict[str, dict[str, Any]] = {}
    results: dict[str, Any] = {}

    def step(name: str, fn: Callable[[], Any]) -> Any:
        f = failures.setdefault(step_source(name, season, prior), {"attempts": 0, "failed": 0, "detail": None})
        f["attempts"] += 1
        try:
            return fn()
        except Exception as e:  # one failing source must not sink the command
            ctx.warnings.append(f"{name} unavailable ({_short(e)})")
            f["failed"] += 1
            f["detail"] = f["detail"] or f"unavailable ({_short(e)})"
            return None

    # 1. schedule ---------------------------------------------------------------
    wk = step("NHL week schedule", lambda: client.week_schedule(ctx.as_of))
    games = step("NHL team schedules", lambda: client.league_games(season, teams))
    sched = {t: [g.date for g in gs] for t, gs in games.items()} if games else None
    if sched:
        ctx.schedule = {t: sorted(ds) for t, ds in sched.items()}
        ctx.games_per_day = games_per_day(ctx.schedule)
        ctx.opponents = {t: {g.date: (g.away if g.home == t else f"@{g.home}") for g in gs}
                         for t, gs in games.items()}
    starts = [d for ds in ctx.schedule.values() for d in ds]
    ctx.season_start = (wk.regular_season_start if wk and wk.regular_season_start else None) or \
        (min(starts) if starts else None)
    if ctx.schedule:
        ctx.source_notes.append(f"Schedule {season}: {sum(ctx.games_per_day.values())} games, "
                                f"regular season starts {ctx.season_start}")

    # 2. NHL season rows and rosters ---------------------------------------------
    cur_sk = step(f"NHL skater stats {season}", lambda: client.all_skaters(season)) or []
    cur_g = step(f"NHL goalie stats {season}", lambda: client.goalie_summary(season)) or []
    pr_sk = step(f"NHL skater stats {prior}", lambda: client.all_skaters(prior)) or []
    pr_g = step(f"NHL goalie stats {prior}", lambda: client.goalie_summary(prior)) or []
    # seasons N-2 / N-3 for the 3-season baseline: one league-wide pull per season (30-day cache)
    hist_rows: dict[int, dict[int, Any]] = {}
    for hs in (prior2, prior3):
        sk = step(f"NHL skater stats {hs}", lambda hs=hs: client.all_skaters(hs)) or []
        gl = step(f"NHL goalie stats {hs}", lambda hs=hs: client.goalie_summary(hs)) or []
        hist_rows[hs] = {r.player_id: r for r in [*sk, *gl]}
    roster: list[NhlRosterPlayer] = []
    roster_errors = 0
    for t in teams:
        try:
            roster.extend(client.team_roster(t, season))
        except Exception as e:
            roster_errors += 1
            last_err = e
    if roster_errors:
        ctx.warnings.append(f"NHL rosters: {roster_errors}/{len(teams)} teams unavailable ({_short(last_err)})")
        failures["NHL rosters"] = {"attempts": len(teams), "failed": roster_errors,
                                   "detail": f"{roster_errors} of {len(teams)} teams unavailable ({_short(last_err)})"}
    goalies: dict[str, dict[int, str]] = {}
    for r in roster:
        if r.position == "G":
            goalies.setdefault(r.team, {})[r.player_id] = r.name
    if goalies:
        ctx.nhl_goalies = goalies

    # 3. crosswalk ------------------------------------------------------------------
    cands = nhl_candidates(pr_sk, pr_g, cur_sk, cur_g, roster)
    if cands:
        xw = crosswalk
        try:
            xw = xw or Crosswalk(settings.fm_data_dir)
            res = xw.resolve(players, cands)
            hint = " (fm sync --review)" if res.pending else ""
            ctx.source_notes.append(f"NHL ids: {res.summary()}{hint}")
        except Exception as e:
            ctx.warnings.append(f"crosswalk failed ({_short(e)})")
        finally:
            if own_xw and xw is not None:
                xw.close()

    # 4. birth dates / teams from rosters ----------------------------------------------
    by_roster = {r.player_id: r for r in roster}
    for p in players:
        r = by_roster.get(p.nhl_id) if p.nhl_id is not None else None
        if r is None:
            continue
        if p.birth_date is None and r.birth_date:
            p.birth_date = r.birth_date
        if not p.team:
            p.team = r.team

    # 5. injuries ------------------------------------------------------------------------
    reports = step("Injury feed", lambda: fetch_injuries(injuries_fetch or fetch))
    if reports is not None:
        n = merge_injuries(players, reports)
        ctx.source_notes.append(f"Injuries: {len(reports)} reports, {n} matched to league players")

    # 6. stat lines ---------------------------------------------------------------------
    cur_rows = {r.player_id: r for r in [*cur_sk, *cur_g]}
    pr_rows = {r.player_id: r for r in [*pr_sk, *pr_g]}
    if cur_rows or pr_rows or any(hist_rows.values()):
        filled = fill_stat_lines(players, cur_rows, pr_rows, hist_rows.get(prior2), hist_rows.get(prior3))
        ctx.source_notes.append(f"NHL stats: {len(cur_rows)} rows {season}, {len(pr_rows)} rows {prior}, "
                                f"{len(hist_rows.get(prior2, {}))} rows {prior2}, "
                                f"{len(hist_rows.get(prior3, {}))} rows {prior3}; "
                                f"filled {filled['season']} season / {filled['prior']} prior / "
                                f"{filled['prior2']} prior2 / {filled['prior3']} prior3 lines")

    # 6b. pedigree (needs prior lines from step 6) ------------------------------------------
    landings: dict[int, Any] = {}
    step("NHL player pages", lambda: _pedigree(ctx, client, players, pedigree_limit, cache, landings))

    # 6c. rookie evidence: league history (reuses 6b's landings) + news role signals -----------
    if rookies and cache is not None:
        step("Rookie evidence", lambda: _rookie_step(ctx, settings, client, cache, tracker, landings))

    # 7. deep: recent form from game logs ---------------------------------------------------
    if deep:
        _deep_game_logs(ctx, client, season, players, deep_fa_limit)

    # 7b. preseason lines of unproven skaters (box scores; needs career GP from 6b) ---------------
    if preseason and cache is not None:
        from .preseason_enrich import enrich_preseason
        results["preseason"] = step("NHL preseason box scores",
                                    lambda: enrich_preseason(ctx, cache, None, season, teams=teams))

    if tracker:
        ctx.source_notes.extend(tracker.notes())

    # 8-10. deployment, lines, expected goals (each best effort) -------------------------------
    if deployment:
        dep_nhl = nhl if nhl is not None else (
            NhlClient(fetch_json=CachedNhlFetch(cache, today=ctx.as_of, ttl_for=ttl_for), season=season)
            if cache is not None else None)
        n_warn = len(ctx.warnings)
        results["deployment"] = step("Deployment", lambda: _deployment_step(ctx, settings, dep_nhl, ledger))
        results["deployment_warnings"] = [w for w in ctx.warnings[n_warn:] if w.startswith("Deployment (")]
    if lines:
        if cache is None and lines_client is None:
            ctx.source_notes.append("Daily Faceoff lines skipped (no HTTP cache)")
        else:
            results["lines"] = step("Daily Faceoff lines",
                                    lambda: _lines_step(ctx, settings, cache, crosswalk, lines_client))
    if xg:
        if cache is None and xg_client is None:
            ctx.source_notes.append("MoneyPuck expected goals skipped (no HTTP cache)")
        else:
            results["xg"] = step("MoneyPuck expected goals", lambda: _xg_step(ctx, cache, xg_client))
    try:  # additive and best effort: never let the health summary sink enrichment
        ctx.sources = collect_sources(ctx, settings, cache, tracker, failures, results)
    except Exception as e:
        ctx.warnings.append(f"Data health summary unavailable ({_short(e)})")
    return ctx


# --------------------------------------------------------------------------- steps 8-10

def _cached_at(cache: Any, url: str, headers: dict | None = None) -> float | None:
    """When ``url`` was fetched into the HTTP cache (epoch seconds), None if unknown."""
    lookup = getattr(cache, "_lookup", None)
    if lookup is None:
        return None
    try:
        row = lookup(cache_key("GET", url, None, headers))
    except Exception:
        return None
    return float(row[3]) if row else None


def _open_ledger(settings: Any) -> Any:
    """The harness ledger when ``<fm_data_dir>/harness.db`` exists (never created here)."""
    from pathlib import Path

    from ..harness.ledger import DB_NAME, Ledger

    data_dir = getattr(settings, "fm_data_dir", None)
    if not data_dir or not (Path(data_dir) / DB_NAME).exists():
        return None
    return Ledger(data_dir)


MIN_LEDGER_TEAM_GAMES = 5


def _deployment_step(ctx: LeagueContext, settings: Any, nhl: Any, ledger: Any = None) -> dict[str, Any]:
    """Step 8: TOI / PP deployment per skater, and (ledger with >= 5 team games) goalie starts
    and back-to-back second-night rates for valuation."""
    from ..harness.deployment import b2b_second_night_rate, team_goalie_counts
    from .deployment_enrich import enrich_deployment

    own = ledger is None
    led = _open_ledger(settings) if own else ledger
    info: dict[str, Any] = {"ledger": led is not None, "last_game": None, "pulled_at": None}
    try:
        res = enrich_deployment(ctx, led, as_of=ctx.as_of, nhl=nhl)
        ctx.deployment_details = dict(res.get("details") or {})
        counts = team_goalie_counts(led, ctx.as_of) if led is not None else {}
        ready = {t: d for t, d in counts.items() if d["games"] >= MIN_LEDGER_TEAM_GAMES}
        for t in ready:
            ctx.b2b_second_night[t] = b2b_second_night_rate(led, t, ctx.as_of, counts=counts)
        for p in ctx.all_players():
            team = normalize_team(p.team)
            if not p.is_goalie or p.nhl_id is None or team not in ready:
                continue
            d = ready[team]
            ctx.goalie_actual_starts[p.cid] = (int(d["starts"].get(int(p.nhl_id), 0)), int(d["games"]))
        fresh = ""
        if led is not None:
            last = led.query("SELECT MAX(game_date) d, MAX(pulled_at) t FROM deployment_pulls")
            if last and last[0]["d"]:
                fresh = f"; ledger through {last[0]['d']}"
                info.update(last_game=last[0]["d"], pulled_at=last[0]["t"])
        src = "ledger" if led is not None else "no harness ledger"
        ctx.source_notes.append(
            f"Deployment (NHL TOI / PP reports, {src}): {res['skaters']} skaters - {res['ledger']} from the ledger, "
            f"{res['season_report']} from the season report, {res['prior_season']} from last season, "
            f"{res['missing']} missing; {res['trends']} with trends; {len(ready)} teams with >= "
            f"{MIN_LEDGER_TEAM_GAMES} ledger games (goalie starts / back-to-backs){fresh}")
        info["result"] = res
    finally:
        if own and led is not None:
            led.close()
    return info


def _lines_step(ctx: LeagueContext, settings: Any, cache: Any, crosswalk: Crosswalk | None,
                client: Any = None) -> Any:
    """Step 9: Daily Faceoff lines / units / starting goalies (and the daily snapshot)."""
    from .dailyfaceoff import USER_AGENT, goalies_url
    from .lines_enrich import enrich_lines

    own = crosswalk is None
    xw = crosswalk
    try:
        if own:
            try:
                xw = Crosswalk(settings.fm_data_dir)
            except Exception as e:  # match without persisting
                ctx.warnings.append(f"Daily Faceoff crosswalk unavailable ({_short(e)})")
                xw = None
        res = enrich_lines(ctx, cache, xw, as_of=ctx.as_of, snapshot_store=getattr(settings, "fm_data_dir", None),
                           client=client)
    finally:
        if own and xw is not None:
            xw.close()
    if cache is not None:
        t = _cached_at(cache, goalies_url(ctx.as_of), {"User-Agent": USER_AGENT})
        if t is not None:
            ctx.source_notes.append(f"Daily Faceoff (dailyfaceoff.com; lines cached 12h, starting goalies 3h): "
                                    f"starting goalies fetched {_age(time.time() - t)}")
    return res


def _xg_step(ctx: LeagueContext, cache: Any, client: Any = None) -> Any:
    """Step 10: MoneyPuck expected goals (credit line added by ``enrich_xg``)."""
    from .moneypuck import current_start_year, season_url
    from .xg_enrich import enrich_xg

    res = enrich_xg(ctx, cache, client=client)
    if cache is not None and res.count:
        t = _cached_at(cache, season_url(current_start_year(ctx.as_of)))
        if t is not None:
            ctx.source_notes.append(f"MoneyPuck: {res.count} skaters with xG; current-season file fetched "
                                    f"{_age(time.time() - t)}")
    return res


def _rookie_llm(settings: Any) -> Any:
    """The free-only LLM client for the news-role pass, or None (no key, or FM_NEWS_ROLES_LLM=0)."""
    import os

    if os.environ.get("FM_NEWS_ROLES_LLM", "1").strip().lower() in ("0", "false", "no", "off"):
        return None
    if not getattr(settings, "openrouter_api_key", None):
        return None
    try:
        from ..llm.openrouter import LLMClient

        client = LLMClient(settings, timeout=30.0)
        return client if client.available else None
    except Exception:
        return None


def _rookie_step(ctx: LeagueContext, settings: Any, client: NhlClient, cache: Any, tracker: Any,
                 landings: dict[int, Any]) -> None:
    """Step 6c (``providers.rookie_enrich``): league history for unproven skaters (6b's landings
    first, then <= 200 uncached pages) and news role signals. Needs the HTTP cache (the caller
    skips it without one, so tests and offline runs never reach the network by accident)."""
    from .rookie_enrich import HISTORY_LIMIT, enrich_rookies

    enrich_rookies(ctx, client, landings=landings, fetch_text=tracker.fetch_text if tracker is not None else None,
                   llm=_rookie_llm(settings), store_dir=getattr(settings, "fm_data_dir", None),
                   limit=HISTORY_LIMIT, is_cached=lambda nid: _landing_cached(cache, nid))


def needs_pedigree(p: Player, as_of: date) -> bool:
    """Young (<= PEDIGREE_MAX_AGE, or age unknown) or unproven (no prior line with at least
    PEDIGREE_PROVEN_GP games) players whose draft pedigree has not been loaded yet."""
    if p.nhl_id is None or p.draft_overall is not None or p.career_gp is not None:
        return False
    age = (as_of - p.birth_date).days / 365.25 if p.birth_date else None
    if age is None or age <= PEDIGREE_MAX_AGE:
        return True
    return p.gp("prior") < PEDIGREE_PROVEN_GP


def pedigree_targets(ctx: LeagueContext, players: list[Player],
                     fa_limit: int = PEDIGREE_FA_LIMIT) -> list[Player]:
    """Players to look up: every rostered one, then the `fa_limit` most-owned free agents."""
    rostered = {p.cid for t in ctx.teams for p in t.players}
    todo = [p for p in players if needs_pedigree(p, ctx.as_of)]
    todo.sort(key=lambda p: (p.cid not in rostered, -(p.pct_owned or 0.0)))
    n_rostered = sum(1 for p in todo if p.cid in rostered)
    return todo[:n_rostered + max(0, fa_limit)]


def apply_landing(p: Player, landing: Any) -> None:
    p.draft_overall = landing.draft_overall
    p.draft_round = landing.draft_round
    p.draft_year = landing.draft_year
    p.career_gp = landing.career_gp
    if p.birth_date is None and landing.birth_date:
        p.birth_date = landing.birth_date


def _landing_cached(cache: Any, player_id: int) -> bool:
    """True when the player's landing page is in the HTTP cache and still fresh."""
    lookup = getattr(cache, "_lookup", None)
    if lookup is None:
        return False
    try:
        row = lookup(cache_key("GET", f"{WEB_BASE}/player/{player_id}/landing", None, None))
    except Exception:
        return False
    return bool(row) and (getattr(cache, "offline", False) or time.time() - float(row[3]) < TTL_LANDING)


def _pedigree(ctx: LeagueContext, client: NhlClient, players: list[Player], limit: int,
              cache: Any = None, landings: dict[int, Any] | None = None) -> None:
    """Load pedigree for every target; only uncached pages count toward `limit` network
    requests per run (the 30-day cache fills the rest in over later runs). ``landings`` collects
    the parsed pages by NHL id (the rookie step reuses their ``seasonTotals``)."""
    todo = pedigree_targets(ctx, players)
    done = errors = drafted = fetched = skipped = 0
    for p in todo:
        if not _landing_cached(cache, p.nhl_id):
            if fetched >= limit:
                skipped += 1
                continue
            fetched += 1
        try:
            landing = client.player_landing(p.nhl_id)
        except Exception:
            errors += 1
            continue
        apply_landing(p, landing)
        if landings is not None:
            landings[int(p.nhl_id)] = landing
        done += 1
        drafted += landing.draft_overall is not None
    if todo:
        ctx.source_notes.append(f"Pedigree: {done}/{len(todo)} young or unproven players loaded, "
                                f"{drafted} drafted" + (f", {errors} failed" if errors else "")
                                + (f", {skipped} deferred to the next run (limit {limit} requests)"
                                   if skipped else ""))


def _deep_game_logs(ctx: LeagueContext, client: NhlClient, season: int, players: list[Player],
                    fa_limit: int) -> None:
    if ctx.season_start and ctx.as_of < ctx.season_start:
        ctx.source_notes.append(f"Game logs skipped: regular season starts {ctx.season_start}")
        return
    rostered = [p for t in ctx.teams for p in t.players]
    fas = sorted(ctx.free_agents, key=lambda p: -(p.pct_owned or 0.0))[:fa_limit]
    todo = [p for p in {p.cid: p for p in rostered + fas}.values()
            if p.nhl_id is not None and not any(s in p.lines for s in ("last7", "last15", "last30"))]
    built = errors = 0
    for p in todo:
        try:
            lines = recent_lines(client.game_log(p.nhl_id, season), ctx.as_of)
        except Exception:
            errors += 1
            continue
        if lines:
            p.lines.update(lines)
            built += 1
    ctx.source_notes.append(f"Game logs: {len(todo)} players checked, {built} with recent games"
                            + (f", {errors} failed" if errors else ""))


# --------------------------------------------------------------------------- data health (ctx.sources)

STEP_SOURCES = {"Injury feed": "Injuries", "NHL player pages": "NHL player pages", "Rookie evidence": "Rookie evidence",
                "NHL preseason box scores": "NHL preseason", "Deployment": "Deployment ledger",
                "Daily Faceoff lines": "Daily Faceoff lines", "MoneyPuck expected goals": "MoneyPuck"}
_STATS_STEP_RE = re.compile(r"NHL (?:skater|goalie) stats (\d{8})$")
_RANK = {"ok": 0, "warn": 1, "fail": 2}


def step_source(name: str, season: int | None = None, prior: int | None = None) -> str:
    """SourceStatus name of an ``enrich_context`` step ("NHL skater stats 20252026" -> "NHL stats")."""
    if name.startswith(("NHL week schedule", "NHL team schedules")):
        return "NHL schedules"
    m = _STATS_STEP_RE.match(name)
    if m:
        return "NHL stats" if int(m.group(1)) in (season, prior) else "NHL stats (past seasons)"
    return STEP_SOURCES.get(name, name)


def _status(name: str, severity: str = "ok", *, fetched: float | None = None, now: float | None = None,
            ttl: float | None = None, detail: str | None = None, requests: int | None = None,
            stale: bool | None = None) -> SourceStatus:
    now = now or time.time()
    return SourceStatus(name=name, ok=severity != "fail", severity=severity,  # type: ignore[arg-type]
                        fetched_at=_utc(fetched), age_seconds=None if fetched is None else max(0.0, now - fetched),
                        ttl_seconds=ttl, detail=detail, requests=requests, feeds_valuation=name in VALUATION_SOURCES,
                        stale=stale)


# Provider warnings (FantraxProvider / EspnProvider ``warnings``, copied onto ctx.warnings by the
# CLI and the web loader before enrichment). Login trouble and core fetch failures are "fail";
# harness extras (activity feeds, box scores, trending) and parse problems are "warn"; the rest
# (config notes such as FANTRAX_POINTS) is not a data failure.
_LOGIN_RE = re.compile(r"not logged in|log ?in\b|cookie|espn_s2|swid|\b401\b|\b403\b|unauthori[sz]ed|forbidden|"
                       r"session (?:expired|invalid)", re.I)
_FAILED_RE = re.compile(r"unavailable|failed|could not reach|not reachable|timed? ?out", re.I)
_PARSE_RE = re.compile(r"could not be parsed", re.I)
_EXTRA_RE = re.compile(r"activity|box scores|trending|lineup snapshot|pro schedule|claim limits|move limits|"
                       r"% rostered|ownership", re.I)
_FANTRAX_LABELS = ("Fantrax", "Standings", "League rules", "Trade blocks", "Pending trades", "Transactions",
                   "Some Fantrax", "Free-agent pool")
LOGIN_ADVICE = {"fantrax": "login expired — refresh FANTRAX_COOKIE (or run `fm auth fantrax --login`)",
                "espn": "login expired — refresh ESPN_S2 / ESPN_SWID"}


def provider_label(provider: str) -> str:
    return {"fantrax": "Fantrax", "espn": "ESPN"}.get((provider or "").lower(), (provider or "League").title())


def _provider_warning(provider: str, w: str) -> bool:
    if provider == "fantrax":
        return w.startswith(_FANTRAX_LABELS)
    if provider == "espn":
        return w.startswith("ESPN ")
    return False


def _clip(text: str, n: int = 120) -> str:
    text = " ".join(str(text).split())
    return text if len(text) <= n else text[: n - 3].rstrip() + "..."


def classify_provider_warning(provider: str, w: str) -> tuple[str, str] | None:
    """(severity, detail) for one provider warning, or None when it is not a data failure."""
    provider = (provider or "").lower()
    if not _provider_warning(provider, w):
        return None
    if "session refreshed by login" in w:
        return "ok", "session refreshed by login"
    if _LOGIN_RE.search(w) and (_FAILED_RE.search(w) or "not logged in" in w.lower()):
        return "fail", LOGIN_ADVICE.get(provider, "login expired")
    if _PARSE_RE.search(w):
        return "warn", _clip(w)
    if _FAILED_RE.search(w):
        return ("warn" if _EXTRA_RE.search(w) else "fail"), _clip(w)
    return None


def provider_status(provider: str, warnings: Iterable[str]) -> SourceStatus:
    """The league provider's SourceStatus from its warnings (the load itself succeeded, or there
    would be no context). The worst warning wins and its detail is kept."""
    severity, detail = "ok", None
    for w in warnings:
        c = classify_provider_warning(provider, str(w))
        if c is None:
            continue
        if _RANK[c[0]] > _RANK[severity] or (detail is None and c[0] == severity):
            severity, detail = c
    return _status(provider_label(provider), severity, detail=detail or "league data loaded")


def _cached_min(cache: Any, urls: Iterable[str]) -> float | None:
    times = [t for t in (_cached_at(cache, u) for u in urls) if t is not None]
    return min(times) if times else None


def _lines_sources(ctx: LeagueContext, cache: Any, res: Any, now: float) -> list[SourceStatus]:
    from .dailyfaceoff import TEAM_SLUGS, TTL_GOALIES, TTL_LINES, goalies_url, team_url

    warns = [str(w) for w in getattr(res, "warnings", None) or []]
    t_lines = _cached_min(cache, (team_url(t) for t in TEAM_SLUGS)) if cache is not None else None
    t_goalies = _cached_at(cache, goalies_url(ctx.as_of)) if cache is not None else None
    full = [w for w in warns if w.startswith("Daily Faceoff lines unavailable (")]
    part = [w for w in warns if w.startswith("Daily Faceoff lines unavailable for")]
    teams = int(getattr(res, "teams", 0) or 0)
    if not teams:
        sev, detail = "fail", (full[0].replace("Daily Faceoff lines ", "", 1) if full else "no line combinations found")
    elif part:
        sev, detail = "warn", _clip(part[0].replace("Daily Faceoff lines ", "", 1))
    else:
        sev, detail = "ok", f"{teams} teams"
    out = [_status("Daily Faceoff lines", sev, fetched=t_lines, now=now, ttl=TTL_LINES, detail=detail)]
    g_fail = [w for w in warns if w.startswith("Daily Faceoff starting goalies unavailable")]
    if g_fail:
        out.append(_status("Daily Faceoff goalies", "fail", fetched=t_goalies, now=now, ttl=TTL_GOALIES,
                           detail=g_fail[0].replace("Daily Faceoff starting goalies ", "", 1)))
    else:
        named = sum(1 for s in getattr(res, "starts", None) or [] if getattr(s, "is_start", False))
        out.append(_status("Daily Faceoff goalies", fetched=t_goalies, now=now, ttl=TTL_GOALIES,
                           detail=f"{named} starters named for {ctx.as_of}"))
    return out


def _season_label(y: int) -> str:
    return f"{y}-{str(y + 1)[-2:]}"


def _xg_source(ctx: LeagueContext, cache: Any, res: Any, now: float) -> SourceStatus:
    from .moneypuck import TTL_CURRENT, season_url

    cur_y, prior_y = res.seasons
    t = _cached_at(cache, season_url(cur_y)) if cache is not None else None
    missing = [y for y in (cur_y, prior_y) if y not in res.rows]
    err = res.errors[0].split(": ", 1)[-1] if res.errors else "no rows"
    if len(missing) == 2:
        sev, detail = "fail", f"unavailable ({_clip(err, 100)})"
    elif cur_y in missing:
        sev, detail = "warn", f"{_season_label(cur_y)} file unavailable ({_clip(err, 80)}); using {_season_label(prior_y)} only"
    elif missing:
        sev, detail = "warn", f"{_season_label(prior_y)} file unavailable ({_clip(err, 80)})"
    else:
        sev, detail = "ok", f"{res.count} skaters with xG"
    return _status("MoneyPuck", sev, fetched=t, now=now, ttl=TTL_CURRENT, detail=detail)


def _parse_local(ts: Any) -> float | None:
    """Epoch seconds of an ISO timestamp (naive = local time, as the harness ledger writes it)."""
    if not ts:
        return None
    try:
        dt = datetime.fromisoformat(str(ts).replace("Z", "+00:00"))
    except ValueError:
        return None
    return (dt if dt.tzinfo else dt.astimezone()).timestamp()


def _deployment_source(ctx: LeagueContext, info: dict[str, Any], report_warnings: list[str],
                       now: float) -> SourceStatus:
    """The harness ledger, judged by missed game days (games before yesterday not pulled yet)
    rather than by age: in the off-season an old ledger is still up to date."""
    sev = "warn" if report_warnings else "ok"
    extra = f"; {_clip(report_warnings[0], 100)}" if report_warnings else ""
    if not info.get("ledger"):
        return _status("Deployment ledger", sev, now=now, detail=f"no harness ledger; NHL season reports only{extra}",
                       stale=False)
    last_s = info.get("last_game")
    try:
        last = date.fromisoformat(str(last_s)) if last_s else None
    except ValueError:
        last = None
    cutoff = ctx.as_of - timedelta(days=1)
    missed = sorted(d for d in ctx.games_per_day if (last is None or d > last) and d < cutoff)
    detail = f"through {last}" if last else "no games pulled yet"
    if missed:
        detail += f"; {len(missed)} game day(s) not pulled since (run `fm harness daily`)"
    return _status("Deployment ledger", sev, fetched=_parse_local(info.get("pulled_at")), now=now,
                   detail=detail + extra, stale=bool(missed))


def _preseason_source(res: dict[str, Any], now: float) -> SourceStatus | None:
    games, failed = int(res.get("games") or 0), int(res.get("failed") or 0)
    if not games and not failed:
        return None                       # skipped (out of window / no unproven skaters / no games yet)
    if failed and not games:
        return _status("NHL preseason", "fail", now=now, detail=f"all {failed} box scores failed")
    return _status("NHL preseason", "warn" if failed else "ok", now=now,
                   detail=f"{games}/{games + failed} box scores read")


def _odds_source(settings: Any, as_of: date, now: float) -> SourceStatus | None:
    """The newest betting-odds archive, listed only when an odds API key is configured."""
    import json
    from pathlib import Path

    if getattr(settings, "odds_api_key", None) is None or not getattr(settings, "fm_data_dir", None):
        return None
    arch = Path(settings.fm_data_dir) / "archive"
    files = sorted(p for p in arch.glob("odds-*.json") if p.stem[5:] <= as_of.isoformat()) if arch.is_dir() else []
    if not files:
        return _status("Odds archive", "warn", now=now, detail="no odds archived yet (`fm harness daily`)")
    path = files[-1]
    try:
        fetched = _parse_local(json.loads(path.read_text(encoding="utf-8")).get("fetched_at"))
    except (OSError, ValueError, AttributeError):
        fetched = None
    if fetched is None:
        try:
            fetched = path.stat().st_mtime
        except OSError:
            fetched = None
    return _status("Odds archive", fetched=fetched, now=now, detail=f"latest {path.stem[5:]}")


def collect_sources(ctx: LeagueContext, settings: Any, cache: Any, tracker: SourceTracker | None,
                    failures: dict[str, dict[str, Any]], results: dict[str, Any],
                    now: float | None = None) -> list[SourceStatus]:
    """``ctx.sources`` for this run: the league provider (from its warnings), every source the
    tracker saw, failed steps, and the Daily Faceoff / MoneyPuck / deployment / preseason / odds
    steps. The provider comes first, the rest by name."""
    now = now or time.time()
    out: dict[str, SourceStatus] = {}
    prov = provider_status(ctx.provider, ctx.warnings)
    if tracker is not None:
        for st in tracker.statuses(now):
            out[st.name] = st
    for name, f in failures.items():
        if not f.get("failed"):
            continue
        sev = "fail" if f["failed"] >= f["attempts"] else "warn"
        cur = out.get(name)
        if cur is None:
            out[name] = _status(name, sev, now=now, detail=f.get("detail"))
        elif _RANK[sev] >= _RANK[cur.severity]:
            out[name] = cur.model_copy(update={"severity": sev, "ok": sev != "fail",
                                               "detail": f.get("detail") or cur.detail})
    if results.get("lines") is not None:
        for st in _lines_sources(ctx, cache, results["lines"], now):
            out[st.name] = st
    elif (failures.get("Daily Faceoff lines") or {}).get("failed"):   # the step raised: no goalies either
        out.setdefault("Daily Faceoff goalies", _status("Daily Faceoff goalies", "fail", now=now,
                                                        detail="unavailable (Daily Faceoff step failed)"))
    if results.get("xg") is not None:
        out["MoneyPuck"] = _xg_source(ctx, cache, results["xg"], now)
    if results.get("deployment") is not None:
        out["Deployment ledger"] = _deployment_source(ctx, results["deployment"],
                                                      results.get("deployment_warnings") or [], now)
    if isinstance(results.get("preseason"), dict):
        st = _preseason_source(results["preseason"], now)
        if st is not None:
            out[st.name] = st
    odds = _odds_source(settings, ctx.as_of, now)
    if odds is not None:
        out[odds.name] = odds
    out.pop(prov.name, None)
    return [prov, *sorted(out.values(), key=lambda s: s.name.lower())]
