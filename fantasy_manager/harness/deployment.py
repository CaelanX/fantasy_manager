"""NHL deployment in the ledger: per-game ice time, power-play share and goalie starts.

* ``pull_deployment``      league-wide per-game rows for a game date (or a date range) from the
                           stats REST ``isGame=true`` reports into ``deployment_daily`` (skaters)
                           and ``goalie_starts`` (goalies); idempotent upserts, and a date without
                           games is recorded in ``deployment_pulls`` with 0 rows.
* ``deployment_summary``   one skater's season-to-date and recent (last N GP) TOI / PP share and
                           the trend of the recent window against a baseline.
* ``goalie_start_counts`` / ``team_goalie_counts``  this season's starts per goalie and games per
                           team, for ``valuation.schedule.start_share(actual=...)``.
* ``b2b_second_night_rate`` how often a team's #1 goalie starts the second night of a
                           back-to-back (``valuation.schedule.week_start_share``).

Units: minutes (TOI) and shares 0..1. A team's PP time in a game comes from the team
powerplay report, else the sum of its skaters' PP seconds / 5 (identical on every game checked);
a skater's ``pp_share`` is his PP time over the team's (None when the team had no power play).

Trend (``toi_trend`` / ``pp_share_trend``) = recent window (last ``last_n`` GP) minus the
baseline: the season's games before that window when there are at least ``MIN_BASE_GP`` of them,
otherwise the ``prior`` averages passed in (typically last season's), otherwise None. It needs a
full window (``gp >= last_n``). Season boundaries: games from 1 September of the season's first
year up to and including ``as_of``.
"""
from __future__ import annotations

from collections import defaultdict
from datetime import date, timedelta
from typing import Any, Iterable, Mapping, Sequence

from ..providers.nhl import current_season
from ..valuation.schedule import B2B_MIN_SAMPLE, B2B_SECOND_NIGHT_P, B2B_SHRINK_K
from .ledger import Ledger, now_iso

DEFAULT_LAST_N = 5
MIN_BASE_GP = 5
PP_UNITS_ON_ICE = 5.0

SUMMARY_KEYS = ("gp", "toi_avg", "pp_toi_avg", "pp_share_avg", "season_toi", "season_pp_toi", "season_pp_share",
                "baseline_toi", "baseline_pp_share", "baseline_source", "toi_trend", "pp_share_trend", "last_n")


def season_start_date(as_of: date) -> date:
    y = as_of.year if as_of.month >= 9 else as_of.year - 1
    return date(y, 9, 1)


def _min(seconds: float | None) -> float | None:
    return None if seconds is None else round(float(seconds) / 60.0, 4)


def _days(start: date, end: date) -> list[date]:
    return [start + timedelta(days=i) for i in range((end - start).days + 1)]


# --------------------------------------------------------------------------- pull

def _teams_on(ledger: Ledger, day: date) -> set[str] | None:
    """Teams with a goalie row on ``day`` when that date was pulled, else None (unknown)."""
    if not ledger.count("deployment_pulls", "WHERE game_date=?", (day.isoformat(),)):
        return None
    return {r["team"] for r in ledger.query("SELECT DISTINCT team FROM goalie_starts WHERE game_date=?",
                                            (day.isoformat(),)) if r["team"]}


def _teams_playing(day: date, ledger: Ledger, nhl: Any, schedule: Mapping[str, Iterable[date]] | None,
                   pulled: Mapping[date, set[str]]) -> set[str] | None:
    """Teams with a regular-season game on ``day``: the given schedule, rows pulled in this call,
    the ledger (a pulled date), else the NHL week schedule (best effort; None if unknown)."""
    if schedule:
        return {t for t, ds in schedule.items() if day in set(ds)}
    if day in pulled:
        return pulled[day]
    got = _teams_on(ledger, day)
    if got is not None:
        return got
    try:
        wk = nhl.week_schedule(day)
        return {t for g in wk.days.get(day, []) if g.game_type == 2 for t in g.teams()}
    except Exception:  # noqa: BLE001 - the flag is optional
        return None


def pull_deployment(ledger: Ledger, nhl: Any, day: date, end: date | None = None, season: int | None = None,
                    schedule: Mapping[str, Iterable[date]] | None = None,
                    record_empty: bool = True) -> dict[str, Any]:
    """Store every skater's and goalie's per-game deployment for games in [day, end or day].

    Idempotent (upserts keyed by player and game date; re-pulling a day in progress overwrites
    it). ``schedule`` (team -> game dates, e.g. ``ctx.schedule``) decides ``back_to_back``; without
    it the ledger / NHL week schedule is used. Returns counts per kind.

    ``record_empty=False`` (the daily harness run): a date without rows is marked pulled in
    ``deployment_pulls`` only when it is complete: ``schedule`` confirms no regular-season game that
    day, or every scheduled team is in the reports (without a schedule: any rows). A date whose
    games are not final yet (empty or partial reports) is left unmarked and listed under
    ``"not_final"``, so ``back_to_back`` lookups never treat it as a day off; its rows are still
    stored and the next run completes them."""
    end = end or day
    season = season or current_season(day)
    toi = nhl.skater_toi_games(season, day, end)
    try:
        pp = {(r.player_id, r.date): r for r in nhl.skater_pp_games(season, day, end)}
    except Exception:  # noqa: BLE001 - the timeonice report already carries PP seconds
        pp = {}
    team_pp: dict[tuple[str, date], float] = {}
    source = "derived"
    try:
        for t in nhl.team_pp_toi_games(season, day, end):
            if t.team and t.pp_toi is not None:
                team_pp[(t.team, t.date)] = float(t.pp_toi)
        source = "report" if team_pp else source
    except Exception:  # noqa: BLE001
        team_pp = {}
    derived: dict[tuple[str, date], float] = defaultdict(float)
    for r in toi:
        if r.team:
            derived[(r.team, r.date)] += float(r.pp_toi or 0.0)
    goalies = nhl.goalie_games(season, day, end)

    now = now_iso()
    rows = []
    for r in toi:
        key = (r.team, r.date)
        tpp = team_pp.get(key)
        if tpp is None and r.team:
            tpp = derived.get(key, 0.0) / PP_UNITS_ON_ICE
        own = r.pp_toi if r.pp_toi is not None else (pp[(r.player_id, r.date)].pp_toi
                                                      if (r.player_id, r.date) in pp else None)
        share = None
        if own is not None and tpp:
            share = min(1.0, max(0.0, float(own) / float(tpp)))
        elif (r.player_id, r.date) in pp and tpp is None:
            share = pp[(r.player_id, r.date)].pp_share
        rows.append({"nhl_id": r.player_id, "game_date": r.date.isoformat(), "team": r.team, "game_id": r.game_id,
                     "opponent": r.opponent, "toi": _min(r.toi), "ev_toi": _min(r.ev_toi), "pp_toi": _min(own),
                     "sh_toi": _min(r.sh_toi), "shifts": r.shifts, "team_pp_toi": _min(tpp),
                     "pp_share": None if share is None else round(share, 4), "pulled_at": now})
    ledger.upsert("deployment_daily", rows, ("nhl_id", "game_date"))

    pulled: dict[date, set[str]] = defaultdict(set)
    for g in goalies:
        if g.team:
            pulled[g.date].add(g.team)
    for r in toi:
        if r.team:
            pulled[r.date].add(r.team)
    grows = []
    prev_cache: dict[date, set[str] | None] = {}
    for g in goalies:
        prev = g.date - timedelta(days=1)
        if prev not in prev_cache:
            prev_cache[prev] = (pulled.get(prev, set()) if day <= prev <= end and not schedule
                                else _teams_playing(prev, ledger, nhl, schedule, pulled))
        teams_prev = prev_cache[prev]
        b2b = None if teams_prev is None or not g.team else int(g.team in teams_prev)
        grows.append({"nhl_id": g.player_id, "game_date": g.date.isoformat(), "team": g.team, "game_id": g.game_id,
                      "opponent": g.opponent, "started": int(g.started), "sa": g.sa, "sv": g.sv, "ga": g.ga,
                      "toi": _min(g.toi), "back_to_back": b2b, "pulled_at": now})
    ledger.upsert("goalie_starts", grows, ("nhl_id", "game_date"))

    per_day_s: dict[str, int] = defaultdict(int)
    per_day_g: dict[str, int] = defaultdict(int)
    for r in rows:
        per_day_s[r["game_date"]] += 1
    for r in grows:
        per_day_g[r["game_date"]] += 1
    marked, not_final = [], []
    for d in _days(day, end):
        iso = d.isoformat()
        if not record_empty:
            # final = every team the schedule has playing that day is in the reports (no schedule:
            # any rows at all); otherwise leave the date unmarked so a later run completes it
            expected = {t for t, ds in schedule.items() if d in set(ds)} if schedule else None
            seen = pulled.get(d, set())
            final = bool(seen) if expected is None else expected <= seen
            if not final:
                not_final.append(iso)
                continue
        marked.append({"game_date": iso, "n_skaters": per_day_s.get(iso, 0), "n_goalies": per_day_g.get(iso, 0),
                       "pulled_at": now})
    ledger.upsert("deployment_pulls", marked, ("game_date",))
    out = {"start": day.isoformat(), "end": end.isoformat(), "skaters": len(rows), "goalies": len(grows),
           "team_pp_source": source if rows else None}
    if not record_empty:
        out["not_final"] = not_final
    return out


# --------------------------------------------------------------------------- skater summary

def _mean(xs: Sequence[float | None]) -> float | None:
    vals = [float(x) for x in xs if x is not None]
    return sum(vals) / len(vals) if vals else None


def _pp_ratio(rows: Sequence[Mapping[str, Any]]) -> float | None:
    """Time-weighted PP share: sum of own PP minutes / sum of team PP minutes (games with a PP)."""
    own = team = 0.0
    for r in rows:
        tpp = r.get("team_pp_toi")
        if tpp:
            own += float(r.get("pp_toi") or 0.0)
            team += float(tpp)
    return min(1.0, own / team) if team > 0 else None


def _r(x: float | None, nd: int = 3) -> float | None:
    return None if x is None else round(float(x), nd)


def summarize_rows(rows: Sequence[Mapping[str, Any]], last_n: int = DEFAULT_LAST_N,
                   prior: Mapping[str, float | None] | None = None) -> dict[str, Any]:
    """Summary of one skater's season rows (sorted by game date); see the module docstring.

    ``prior`` = {"toi": minutes, "pp_share": 0..1} (e.g. last season) is the trend baseline when
    fewer than ``MIN_BASE_GP`` games precede the recent window."""
    rows = sorted(rows, key=lambda r: r["game_date"])
    out: dict[str, Any] = {k: None for k in SUMMARY_KEYS}
    out.update(gp=len(rows), last_n=last_n)
    if not rows:
        return out
    recent = rows[-last_n:]
    before = rows[:-last_n] if len(rows) > last_n else []
    out.update(toi_avg=_r(_mean([r.get("toi") for r in recent])),
               pp_toi_avg=_r(_mean([r.get("pp_toi") for r in recent])),
               pp_share_avg=_r(_pp_ratio(recent), 4),
               season_toi=_r(_mean([r.get("toi") for r in rows])),
               season_pp_toi=_r(_mean([r.get("pp_toi") for r in rows])),
               season_pp_share=_r(_pp_ratio(rows), 4))
    if len(before) >= MIN_BASE_GP:
        base_toi, base_pp, src = _mean([r.get("toi") for r in before]), _pp_ratio(before), "season"
    elif prior:
        base_toi, base_pp, src = prior.get("toi"), prior.get("pp_share"), "prior"
    else:
        base_toi = base_pp = src = None
    out.update(baseline_toi=_r(base_toi), baseline_pp_share=_r(base_pp, 4), baseline_source=src)
    if len(rows) >= last_n:
        if base_toi is not None and out["toi_avg"] is not None:
            out["toi_trend"] = _r(out["toi_avg"] - base_toi)
        if base_pp is not None and out["pp_share_avg"] is not None:
            out["pp_share_trend"] = _r(out["pp_share_avg"] - base_pp, 4)
    return out


_ROW_COLS = "nhl_id, game_date, team, toi, ev_toi, pp_toi, sh_toi, shifts, team_pp_toi, pp_share"


def season_rows(ledger: Ledger, as_of: date, nhl_ids: Iterable[int] | None = None) -> dict[int, list[dict[str, Any]]]:
    """nhl_id -> this season's deployment rows up to ``as_of`` (sorted by date)."""
    params: list[Any] = [season_start_date(as_of).isoformat(), as_of.isoformat()]
    sql = f"SELECT {_ROW_COLS} FROM deployment_daily WHERE game_date >= ? AND game_date <= ?"
    ids = None if nhl_ids is None else sorted({int(i) for i in nhl_ids})
    if ids is not None:
        if not ids:
            return {}
        if len(ids) <= 500:
            sql += f" AND nhl_id IN ({','.join('?' for _ in ids)})"
            params += ids
    out: dict[int, list[dict[str, Any]]] = defaultdict(list)
    wanted = set(ids) if ids is not None else None
    for r in ledger.query(sql + " ORDER BY nhl_id, game_date", params):
        if wanted is None or r["nhl_id"] in wanted:
            out[int(r["nhl_id"])].append(r)
    return dict(out)


def deployment_summary(ledger: Ledger, nhl_id: int, as_of: date, last_n: int = DEFAULT_LAST_N,
                       prior: Mapping[str, float | None] | None = None) -> dict[str, Any]:
    """{gp, toi_avg, pp_toi_avg, pp_share_avg (last ``last_n`` GP), season_toi, season_pp_toi,
    season_pp_share, baseline_toi, baseline_pp_share, baseline_source, toi_trend, pp_share_trend,
    last_n} for one skater this season up to ``as_of`` (minutes, shares 0..1; None when unknown)."""
    rows = season_rows(ledger, as_of, [nhl_id]).get(int(nhl_id), [])
    return summarize_rows(rows, last_n, prior)


def deployment_summaries(ledger: Ledger, nhl_ids: Iterable[int], as_of: date, last_n: int = DEFAULT_LAST_N,
                         priors: Mapping[int, Mapping[str, float | None]] | None = None) -> dict[int, dict[str, Any]]:
    """:func:`deployment_summary` for many skaters with one query (only ids with rows)."""
    return {pid: summarize_rows(rows, last_n, (priors or {}).get(pid))
            for pid, rows in season_rows(ledger, as_of, nhl_ids).items()}


# --------------------------------------------------------------------------- goalies

def team_goalie_counts(ledger: Ledger, as_of: date) -> dict[str, dict[str, Any]]:
    """team -> {"games": team games this season up to ``as_of`` (dates with a goalie row),
    "starts": {nhl_id: starts for that team}, "b2b": second nights, "b2b_starter": second nights
    the team's #1 (most starts) started, "starter": the #1's nhl_id}."""
    rows = ledger.query("SELECT nhl_id, game_date, team, started, back_to_back FROM goalie_starts "
                        "WHERE game_date >= ? AND game_date <= ? AND team IS NOT NULL",
                        (season_start_date(as_of).isoformat(), as_of.isoformat()))
    out: dict[str, dict[str, Any]] = {}
    dates: dict[str, set[str]] = defaultdict(set)
    b2b_dates: dict[str, set[str]] = defaultdict(set)
    starters_on: dict[tuple[str, str], set[int]] = defaultdict(set)
    for r in rows:
        t = r["team"]
        d = out.setdefault(t, {"games": 0, "starts": defaultdict(int), "b2b": 0, "b2b_starter": 0, "starter": None})
        dates[t].add(r["game_date"])
        if r["started"]:
            d["starts"][int(r["nhl_id"])] += 1
            starters_on[(t, r["game_date"])].add(int(r["nhl_id"]))
        if r["back_to_back"]:
            b2b_dates[t].add(r["game_date"])
    for t, d in out.items():
        d["games"] = len(dates[t])
        d["starts"] = dict(d["starts"])
        if d["starts"]:
            d["starter"] = max(sorted(d["starts"]), key=lambda k: d["starts"][k])
        d["b2b"] = len(b2b_dates[t])
        d["b2b_starter"] = sum(1 for day in b2b_dates[t] if d["starter"] in starters_on.get((t, day), set()))
    return out


def goalie_start_counts(ledger: Ledger, nhl_id: int, as_of: date, team: str | None = None) -> tuple[int, int]:
    """(starts for ``team``, ``team`` games) this season up to ``as_of``; ``team`` defaults to the
    goalie's most recent team in the ledger. (0, 0) when unknown."""
    if team is None:
        last = ledger.query("SELECT team FROM goalie_starts WHERE nhl_id=? AND game_date >= ? AND game_date <= ? "
                            "ORDER BY game_date DESC LIMIT 1",
                            (int(nhl_id), season_start_date(as_of).isoformat(), as_of.isoformat()))
        team = last[0]["team"] if last else None
    if not team:
        return 0, 0
    d = team_goalie_counts(ledger, as_of).get(team)
    if not d:
        return 0, 0
    return int(d["starts"].get(int(nhl_id), 0)), int(d["games"])


def b2b_second_night_rate(ledger: Ledger, team: str, as_of: date,
                          counts: Mapping[str, Mapping[str, Any]] | None = None) -> tuple[float, int]:
    """(probability the team's #1 starts the second night of a back-to-back, second nights seen).

    ``B2B_SECOND_NIGHT_P`` (0.35) until the team has ``B2B_MIN_SAMPLE`` (5) second nights in the
    ledger; then the observed rate shrunk toward 0.35 with k = ``B2B_SHRINK_K``."""
    d = (counts if counts is not None else team_goalie_counts(ledger, as_of)).get(team)
    n = int(d["b2b"]) if d else 0
    if n < B2B_MIN_SAMPLE:
        return B2B_SECOND_NIGHT_P, n
    hits = float(d["b2b_starter"])
    return (hits + B2B_SHRINK_K * B2B_SECOND_NIGHT_P) / (n + B2B_SHRINK_K), n
