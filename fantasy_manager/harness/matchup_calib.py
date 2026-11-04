"""Matchup win-probability calibration: record what ``analysis.matchup`` predicted every day, the
final score once the period ends, and grade the predictions.

* ``record_snapshot``  one ``matchup_snapshots`` row per league per day (the first capture of the
  day wins, so a rerun changes nothing): period, my / their projected final, projected margin, the
  margin's SD (``sqrt(my_sd**2 + their_sd**2)``, the same per-starter SDs the Monte Carlo draws
  from) and the win probability.
* ``fetch_final_scores`` / ``attach_results``  for periods that have ended (``period_end`` before
  today) the provider's final score goes into ``matchup_results``. ESPN: the ``mMatchupScore``
  schedule (only once ESPN names a winner); Fantrax: the ``getStandings`` SCHEDULE tables
  (``analysis.matchup.parse_fantrax_schedule``). Results are re-read for 3 days after the period
  ends (stat corrections), then left alone.
* ``calibration_report``  Brier score, log loss and a reliability table of the recorded win
  probabilities against the results, plus the fitted SD scale ``s``: with
  ``z = (actual margin - projected margin) / margin SD``, the maximum-likelihood scale of a
  normal error is ``s = sqrt(mean z**2)`` (each matchup weighted equally, since a matchup's daily
  snapshots share one outcome). ``s > 1`` means the model's SDs are too small (overconfident win
  probabilities). The scale is only reported once ``MIN_MATCHUPS`` periods are complete, and it
  is never applied: the SD constants live in ``analysis.matchup`` (``SKATER_SD_K`` /
  ``GOALIE_SD_K``), which are not valuation params, so nothing is written to the params store.
"""
from __future__ import annotations

import math
from datetime import date, timedelta
from typing import Any, Iterable, Mapping, Sequence

from .ledger import Ledger, now_iso

MIN_MATCHUPS = 8            # completed matchups before the SD scale is fitted
RELIABLE_MATCHUPS = 20
RECHECK_DAYS = 3            # re-read a final score this many days after the period ends
SD_MIN = 0.5                # snapshots with a smaller margin SD (nothing left to play) are not graded
LOGLOSS_EPS = 1e-3
BINS = 5
SCALE_OK = (0.9, 1.1)       # fitted scale within this band: "about right"


# --------------------------------------------------------------------------- recording

def _num(v: Any) -> float | None:
    if isinstance(v, bool) or v is None:
        return None
    try:
        f = float(str(v).replace(",", "")) if not isinstance(v, (int, float)) else float(v)
    except (TypeError, ValueError):
        return None
    return f if math.isfinite(f) else None


def snapshot_row(league: str, day: date, m: Any) -> dict[str, Any] | None:
    """A ``matchup_snapshots`` row from an ``analysis.matchup.Matchup`` (or anything with its
    attributes); None when there is no opponent or win probability to record."""
    p = getattr(m, "win_probability", None)
    if p is None or getattr(m, "opponent_team", None) is None:
        return None
    my_sd = float(getattr(m, "my_sd", 0.0) or 0.0)
    their_sd = float(getattr(m, "their_sd", 0.0) or 0.0)
    mine = float(getattr(m, "my_points_so_far", 0.0) or 0.0) + float(getattr(m, "my_projected_remaining", 0.0) or 0.0)
    theirs = (float(getattr(m, "their_points_so_far", 0.0) or 0.0)
              + float(getattr(m, "their_projected_remaining", 0.0) or 0.0))
    try:
        from ..analysis import matchup as am
        sk, gk = getattr(am, "SKATER_SD_K", None), getattr(am, "GOALIE_SD_K", None)
    except Exception:  # noqa: BLE001
        sk = gk = None
    start, end = getattr(m, "start", None), getattr(m, "end", None)
    return {"league": league, "day": day.isoformat(), "period": getattr(m, "period", None),
            "period_start": start.isoformat() if start else None, "period_end": end.isoformat() if end else None,
            "opponent_id": None if getattr(m, "opponent_id", None) is None else str(m.opponent_id),
            "opponent": m.opponent_team, "days_left": int(getattr(m, "days_left", 0) or 0),
            "playoffs": int(bool(getattr(m, "playoffs", False))),
            "my_so_far": float(getattr(m, "my_points_so_far", 0.0) or 0.0),
            "their_so_far": float(getattr(m, "their_points_so_far", 0.0) or 0.0),
            "my_proj_total": mine, "their_proj_total": theirs, "margin": mine - theirs,
            "my_sd": my_sd, "their_sd": their_sd, "margin_sd": math.sqrt(my_sd ** 2 + their_sd ** 2),
            "win_prob": float(p), "skater_sd_k": sk, "goalie_sd_k": gk,
            "source": getattr(m, "source", None), "recorded_at": now_iso()}


def record_snapshot(ledger: Ledger, league: str, day: date, m: Any) -> str:
    """Store today's snapshot once: "recorded", "already recorded today" or "nothing to record"."""
    row = snapshot_row(league, day, m)
    if row is None:
        return "nothing to record (no opponent or win probability)"
    cols = list(row)
    cur = ledger.execute(f"INSERT OR IGNORE INTO matchup_snapshots ({', '.join(cols)}) "
                         f"VALUES ({', '.join('?' for _ in cols)})", [row[c] for c in cols])
    ledger.commit()
    return "recorded" if cur.rowcount else "already recorded today"


def pending_periods(ledger: Ledger, league: str, today: date) -> list[dict[str, Any]]:
    """Ended periods (``period_end`` before ``today``) without a result, or still inside the
    re-check window."""
    recheck = (today - timedelta(days=RECHECK_DAYS)).isoformat()
    return ledger.query(
        "SELECT s.period, s.period_start, MAX(s.period_end) period_end FROM matchup_snapshots s "
        "LEFT JOIN matchup_results r ON r.league=s.league AND r.period=s.period AND r.period_end=s.period_end "
        "WHERE s.league=? AND s.period IS NOT NULL AND s.period_end IS NOT NULL AND s.period_end < ? "
        "AND (r.result IS NULL OR s.period_end >= ?) GROUP BY s.period, s.period_end ORDER BY s.period_end",
        (league, today.isoformat(), recheck))


def result_of(mine: float, theirs: float) -> str:
    return "tie" if abs(mine - theirs) < 0.005 else ("win" if mine > theirs else "loss")


def attach_results(ledger: Ledger, league: str, finals: Mapping[int, Mapping[str, Any]],
                   pending: Sequence[Mapping[str, Any]]) -> int:
    """Upsert ``matchup_results`` for every pending period with a final score; returns rows."""
    rows = []
    for p in pending:
        f = finals.get(int(p["period"]))
        if not f or f.get("mine") is None or f.get("theirs") is None:
            continue
        rows.append({"league": league, "period": int(p["period"]), "period_end": p["period_end"],
                     "period_start": p.get("period_start"), "opponent_id": f.get("opponent_id"),
                     "my_final": float(f["mine"]), "their_final": float(f["theirs"]),
                     "result": result_of(float(f["mine"]), float(f["theirs"])), "source": f.get("source"),
                     "recorded_at": now_iso()})
    return ledger.upsert("matchup_results", rows, ["league", "period", "period_end"])


# --------------------------------------------------------------------------- provider final scores

def _espn_finals(ctx: Any, provider: Any, periods: set[int]) -> dict[int, dict[str, Any]]:
    from ..analysis.schedule_grid import espn_league

    league = espn_league(provider)
    data = league.espn_request.league_get(params={"view": "mMatchupScore"}) or {}
    my_id = int(ctx.my_team.team_id)
    out: dict[int, dict[str, Any]] = {}
    for m in data.get("schedule") or []:
        mp = m.get("matchupPeriodId")
        if mp not in periods:
            continue
        home, away = m.get("home") or {}, m.get("away") or {}
        if my_id not in (home.get("teamId"), away.get("teamId")):
            continue
        if str(m.get("winner") or "").upper() == "UNDECIDED":
            continue                                    # ESPN has not finalized the period yet
        me, them = (home, away) if home.get("teamId") == my_id else (away, home)

        def pts(side: Mapping[str, Any]) -> float | None:
            v = _num(side.get("totalPoints"))
            return v if v is not None else _num(side.get("totalPointsLive"))

        out[int(mp)] = {"mine": pts(me), "theirs": pts(them) if them else None,
                        "opponent_id": None if them.get("teamId") is None else str(them["teamId"]),
                        "source": "ESPN scoreboard"}
    return out


def _fantrax_finals(ctx: Any, provider: Any, periods: set[int]) -> dict[int, dict[str, Any]]:
    from ..analysis.matchup import parse_fantrax_schedule

    data = provider._get_client().call(("getStandings", {"view": "SCHEDULE"}))[0]
    my_id = ctx.my_team.team_id
    out: dict[int, dict[str, Any]] = {}
    for t in parse_fantrax_schedule(data):
        if t.get("number") not in periods:
            continue
        for away, apts, home, hpts in t["games"]:
            if my_id not in (away, home):
                continue
            mine_away = away == my_id
            opp = home if mine_away else away
            out[int(t["number"])] = {"mine": apts if mine_away else hpts, "theirs": hpts if mine_away else apts,
                                     "opponent_id": None if opp is None else str(opp), "source": "Fantrax schedule"}
    return out


def fetch_final_scores(ctx: Any, provider: Any, periods: Iterable[int]) -> dict[int, dict[str, Any]]:
    """{period: {mine, theirs, opponent_id, source}} for the given ended periods (raises on
    provider errors; the caller treats them as warnings)."""
    wanted = {int(p) for p in periods if p is not None}
    if not wanted or provider is None:
        return {}
    if ctx.provider == "espn":
        return _espn_finals(ctx, provider, wanted)
    if ctx.provider == "fantrax":
        return _fantrax_finals(ctx, provider, wanted)
    return {}


def capture_matchup(ledger: Ledger, league: str, ctx: Any, provider: Any, values: Any, today: date,
                    matchup_fn: Any = None, finals_fn: Any = None) -> dict[str, Any]:
    """The daily step: snapshot today's matchup, then attach final scores of ended periods.
    Never raises; problems land in ``warnings``."""
    out: dict[str, Any] = {"warnings": []}
    try:
        if matchup_fn is None:
            from ..analysis.matchup import current_matchup as matchup_fn
        m = matchup_fn(ctx, provider, values, today=today, streaming=False, seed=int(today.strftime("%Y%m%d")))
        out["snapshot"] = record_snapshot(ledger, league, today, m)
        if getattr(m, "win_probability", None) is not None:
            out.update(period=getattr(m, "period", None), win_prob=round(float(m.win_probability), 3))
    except Exception as e:  # noqa: BLE001 - the snapshot is optional
        out["snapshot"] = "failed"
        out["warnings"].append(f"matchup snapshot failed: {type(e).__name__}: {e}")
    pend = pending_periods(ledger, league, today)
    out["results"] = 0
    if pend:
        try:
            finals = (finals_fn or fetch_final_scores)(ctx, provider, [p["period"] for p in pend])
            out["results"] = attach_results(ledger, league, finals, pend)
        except Exception as e:  # noqa: BLE001
            out["warnings"].append(f"matchup final scores unavailable: {type(e).__name__}: {e}")
    return out


# --------------------------------------------------------------------------- grading

def outcome_value(result: str) -> float:
    return {"win": 1.0, "loss": 0.0}.get(result, 0.5)


def brier(ps: Sequence[float], ys: Sequence[float]) -> float | None:
    if not ps:
        return None
    return sum((p - y) ** 2 for p, y in zip(ps, ys)) / len(ps)


def log_loss(ps: Sequence[float], ys: Sequence[float], eps: float = LOGLOSS_EPS) -> float | None:
    """Mean binary cross-entropy; probabilities are clipped to [eps, 1 - eps] (the Monte Carlo
    can return exactly 0 or 1), a tie is y = 0.5."""
    if not ps:
        return None
    tot = 0.0
    for p, y in zip(ps, ys):
        p = min(1 - eps, max(eps, p))
        tot -= y * math.log(p) + (1 - y) * math.log(1 - p)
    return tot / len(ps)


def reliability(ps: Sequence[float], ys: Sequence[float], bins: int = BINS) -> list[dict[str, Any]]:
    """Equal-width bins of predicted win probability: n, mean predicted, observed win rate."""
    out = []
    for b in range(bins):
        lo, hi = b / bins, (b + 1) / bins
        idx = [i for i, p in enumerate(ps) if lo <= p < hi or (b == bins - 1 and p == 1.0)]
        out.append({"lo": round(lo, 3), "hi": round(hi, 3), "n": len(idx),
                    "mean_pred": (sum(ps[i] for i in idx) / len(idx)) if idx else None,
                    "observed": (sum(ys[i] for i in idx) / len(idx)) if idx else None})
    return out


def fit_sd_scale(groups: Mapping[Any, Sequence[float]]) -> float | None:
    """ML scale of a zero-mean normal error from standardized errors ``z`` grouped by matchup:
    ``sqrt(mean over matchups of mean z**2)`` (equal weight per matchup)."""
    per = [sum(z * z for z in zs) / len(zs) for zs in groups.values() if zs]
    if not per:
        return None
    return math.sqrt(sum(per) / len(per))


def _phi(x: float) -> float:
    return 0.5 * (1 + math.erf(x / math.sqrt(2)))


def trust_for(n_matchups: int, min_matchups: int = MIN_MATCHUPS) -> str:
    if n_matchups < min_matchups:
        return "hidden"
    return "reliable" if n_matchups >= RELIABLE_MATCHUPS else "provisional"


def graded_rows(ledger: Ledger, league: str) -> list[dict[str, Any]]:
    """Snapshots of completed matchups (period still being played on the snapshot day) joined
    with their result."""
    return ledger.query(
        "SELECT s.*, r.my_final, r.their_final, r.result FROM matchup_snapshots s "
        "JOIN matchup_results r ON r.league=s.league AND r.period=s.period AND r.period_end=s.period_end "
        "WHERE s.league=? AND s.days_left > 0 AND s.win_prob IS NOT NULL ORDER BY s.day", (league,))


def calibration_report(ledger: Ledger, league: str, min_matchups: int = MIN_MATCHUPS) -> dict[str, Any]:
    """Brier, log loss, reliability bins and the fitted SD scale for one league."""
    rows = graded_rows(ledger, league)
    n_snap = ledger.count("matchup_snapshots", "WHERE league=?", (league,))
    last = {(r["period"], r["period_end"]): r for r in rows}          # one row per matchup
    keys = sorted(last)
    ps = [float(r["win_prob"]) for r in rows]
    ys = [outcome_value(r["result"]) for r in rows]
    rep: dict[str, Any] = {
        "league": league, "snapshots": n_snap, "graded_snapshots": len(rows), "matchups": len(keys),
        "min_matchups": min_matchups, "trust": trust_for(len(keys), min_matchups),
        "brier": brier(ps, ys), "log_loss": log_loss(ps, ys),
        "brier_coin_flip": 0.25 if rows else None, "reliability": reliability(ps, ys) if rows else [],
        "record": {k: sum(1 for r in last.values() if r["result"] == k) for k in ("win", "loss", "tie")},
        "scale": None, "status": "no completed matchups yet" if not rows else None,
    }
    # stance check: how often a "chase" (< 0.40) / "protect" (> 0.60) snapshot ended in a win
    try:
        from ..analysis import matchup as am
        lo, hi = float(am.CHASE_BELOW), float(am.PROTECT_ABOVE)
    except Exception:  # noqa: BLE001
        lo, hi = 0.40, 0.60
    for name, sel in (("chase", [y for p, y in zip(ps, ys) if p < lo]),
                      ("protect", [y for p, y in zip(ps, ys) if p > hi])):
        rep[f"{name}_win_rate"] = {"n": len(sel), "rate": (sum(sel) / len(sel)) if sel else None,
                                   "threshold": lo if name == "chase" else hi}
    groups: dict[Any, list[float]] = {}
    fit_rows = [r for r in rows if (r["margin_sd"] or 0) >= SD_MIN]
    for r in fit_rows:
        actual = float(r["my_final"]) - float(r["their_final"])
        groups.setdefault((r["period"], r["period_end"]), []).append((actual - float(r["margin"])) / float(r["margin_sd"]))
    if rows and len(keys) < min_matchups:
        rep["status"] = f"insufficient data ({len(keys)} of {min_matchups} completed matchups)"
    elif rows:
        s = fit_sd_scale(groups)
        if s is None:
            rep["status"] = "insufficient data (no snapshot with a margin SD)"
        else:
            sk = next((r["skater_sd_k"] for r in reversed(rows) if r["skater_sd_k"] is not None), None)
            gk = next((r["goalie_sd_k"] for r in reversed(rows) if r["goalie_sd_k"] is not None), None)
            ps2 = [_phi(float(r["margin"]) / (s * float(r["margin_sd"]))) for r in fit_rows]
            ys2 = [outcome_value(r["result"]) for r in fit_rows]
            verdict = ("overconfident" if s > SCALE_OK[1] else "underconfident" if s < SCALE_OK[0]
                       else "about right")
            rep["scale"] = {"s": s, "n_matchups": len(groups), "n_snapshots": len(fit_rows), "verdict": verdict,
                            "skater_sd_k": sk, "goalie_sd_k": gk,
                            "recommended_skater_sd_k": None if sk is None else sk * s,
                            "recommended_goalie_sd_k": None if gk is None else gk * s,
                            "brier_if_scaled": brier(ps2, ys2), "applied": False}
            rep["status"] = f"SD scale {s:.2f} ({verdict})"
    return rep


def summary_line(rep: Mapping[str, Any]) -> str:
    """One line for the CLI / Health warnings."""
    lg = rep.get("league")
    if not rep.get("graded_snapshots"):
        return f"{lg}: matchup win probability: {rep.get('snapshots', 0)} snapshot(s), no completed matchup yet"
    b, ll = rep.get("brier"), rep.get("log_loss")
    base = (f"{lg}: matchup win probability over {rep['matchups']} matchup(s) / {rep['graded_snapshots']} "
            f"snapshot(s): Brier {b:.3f} (coin flip 0.250), log loss {ll:.3f}")
    sc = rep.get("scale")
    if sc:
        rec = ""
        if sc.get("recommended_skater_sd_k") is not None:
            rec = (f"; suggested SD constants {sc['recommended_skater_sd_k']:.2f} skaters / "
                   f"{sc['recommended_goalie_sd_k']:.2f} goalies (not applied)")
        return base + f"; fitted SD scale {sc['s']:.2f} ({sc['verdict']}){rec}"
    return base + f"; {rep.get('status')}"


__all__ = ["MIN_MATCHUPS", "attach_results", "brier", "calibration_report", "capture_matchup",
           "fetch_final_scores", "fit_sd_scale", "log_loss", "outcome_value", "pending_periods", "record_snapshot",
           "reliability", "result_of", "snapshot_row", "summary_line"]
