"""Everything the /health page, ``/api/health.json`` and the digest's "Model health" block read
(M4 surfaces), built from the ledger and the params versions. Pure reads: nothing is written.

* ``series``    chart series per league: rolling 28-day projection MAE per pool (fm and the four
  baselines, one point per weekly snapshot), skill vs season-to-date per pool (fm and the
  baselines, to-date itself is the zero line) and the weekly (per graded week) hit rate of the
  model's recommendations per kind with its Wilson 95% band.
* ``scorecard`` followed / partial / ignored / user-only counts and hit rates (latest bar).
* ``capture``   what the ledger holds: episodes by kind and status, transactions, realized days,
  unmatched identities, the last daily run and a warning list.
* ``changelog`` the params versions (``params_store.history()``), newest first, with the
  rollback targets; ``calendar`` the refit calendar and the auto-apply preference.
* ``health_block`` the compact 3-line digest block (hidden until something is trustworthy).
"""
from __future__ import annotations

import json
from datetime import date, datetime, timedelta
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence

from .ledger import Ledger
from .metrics import (BASELINES, HIT_KINDS, HIT_PROVISIONAL, POOLS, hit_trust, latest_rows, league_report,
                      need_for, status_report, wilson)

MODEL_ORIGINS = ("followed", "partial", "ignored")
SCORECARD_ORIGINS = ("followed", "partial", "ignored", "user_only")
ROLLBACK_FROM = ("shadow", "retired", "rolled_back")
STALE_RUN_HOURS = 36
TREND_TOLERANCE = 0.01          # MAE within 1% of the previous week: flat


def _decode(r: Mapping[str, Any]) -> dict[str, Any]:
    out = dict(r)
    try:
        out["detail"] = json.loads(out.pop("detail_json", None) or "{}")
    except ValueError:
        out["detail"] = {}
    return out


def history_rows(ledger: Ledger, league: str, metric: str | None = None) -> list[dict[str, Any]]:
    """Every stored ``metric_snapshots`` row of ``league`` (optionally one metric), oldest week first."""
    sql = "SELECT * FROM metric_snapshots WHERE league=?" + (" AND metric=?" if metric else "") + " ORDER BY week, pool"
    return [_decode(r) for r in ledger.query(sql, (league, metric) if metric else (league,))]


# --------------------------------------------------------------------------- chart series

def _proj_series(latest: Sequence[Mapping[str, Any]]) -> tuple[dict[str, Any], dict[str, Any]]:
    mae: dict[str, Any] = {}
    skill: dict[str, Any] = {}
    by = {(r["metric"], r["pool"]): r for r in latest}
    for pool in POOLS:
        m = by.get(("proj_fpg_mae", pool))
        s = by.get(("proj_fpg_skill", pool))
        d = (m or {}).get("detail") or {}
        weeks = [w["week"] for w in d.get("per_week") or []]
        base = {w["week"]: w for w in d.get("per_week_base") or []}
        lines = {"fm": [w.get("mae") for w in d.get("per_week") or []]}
        for b in BASELINES:
            lines[b] = [((base.get(wk) or {}).get("base_mae") or {}).get(b) for wk in weeks]
        mae[pool] = {"trust": (m or {}).get("trust", "hidden"), "n": (m or {}).get("n", 0),
                     "need": d.get("need", need_for("proj_fpg_mae", 0)), "value": (m or {}).get("value"),
                     "x": weeks, "lines": lines, "n_by_week": [w.get("n") for w in d.get("per_week") or []]}
        sk_lines: dict[str, list[float | None]] = {
            "fm": [((base.get(wk) or {}).get("skill") or {}).get("fm") for wk in weeks],
            "to_date": [0.0 if (base.get(wk) or {}).get("n_to_date") else None for wk in weeks]}
        for b in BASELINES:
            if b != "to_date":
                sk_lines[b] = [((base.get(wk) or {}).get("skill") or {}).get(b) for wk in weeks]
        sd = (s or {}).get("detail") or {}
        skill[pool] = {"trust": (s or {}).get("trust", "hidden"), "n": (s or {}).get("n", 0),
                       "need": sd.get("need", need_for("proj_fpg_skill", 0)), "value": (s or {}).get("value"),
                       "ci_lo": (s or {}).get("ci_lo"), "ci_hi": (s or {}).get("ci_hi"),
                       "x": weeks if base else [], "lines": sk_lines if base else {}}
    return mae, skill


def _hit_series(history: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    """Per kind, per graded week: the model's recs (followed + partial + ignored) pooled."""
    weeks = sorted({r["week"] for r in history})
    out: dict[str, Any] = {}
    for kind in HIT_KINDS:
        rate, lo, hi, ns = [], [], [], []
        for wk in weeks:
            rows = [r for r in history if r["week"] == wk and r["pool"] in {f"{kind}:{o}" for o in MODEL_ORIGINS}]
            n = sum(int(r["n"] or 0) for r in rows)
            k = sum(int((r.get("detail") or {}).get("hits") or 0) for r in rows)
            a, b = wilson(k, n)
            rate.append(round(k / n, 4) if n else None)
            lo.append(None if a is None else round(a, 4))
            hi.append(None if b is None else round(b, 4))
            ns.append(n)
        if not any(ns):
            continue
        last_n = ns[-1] if ns else 0
        out[kind] = {"trust": hit_trust(last_n), "n": last_n, "need": max(0, HIT_PROVISIONAL - last_n),
                     "value": rate[-1] if rate else None, "x": weeks, "lines": {"rate": rate},
                     "band": {"lo": lo, "hi": hi}, "n_by_week": ns}
    return out


def series(ledger: Ledger, league: str) -> dict[str, Any]:
    _, latest = latest_rows(ledger, league)
    mae, skill = _proj_series(latest)
    return {"mae": mae, "skill": skill, "hit": _hit_series(history_rows(ledger, league, "hit_rate"))}


# --------------------------------------------------------------------------- scorecard

def scorecard(latest: Sequence[Mapping[str, Any]]) -> list[dict[str, Any]]:
    """Followed / partial / ignored / user-only: counts and hit rates over every kind (trades
    excluded: they are a case list), latest bar."""
    out = []
    for origin in SCORECARD_ORIGINS:
        rows = [r for r in latest if r["metric"] == "hit_rate" and r["pool"].endswith(f":{origin}")]
        n = sum(int(r["n"] or 0) for r in rows)
        k = sum(int((r.get("detail") or {}).get("hits") or 0) for r in rows)
        lo, hi = wilson(k, n)
        out.append({"origin": origin, "n": n, "hits": k, "rate": (k / n) if n else None, "ci_lo": lo, "ci_hi": hi,
                    "trust": hit_trust(n), "need": max(0, HIT_PROVISIONAL - n),
                    "kinds": {r["pool"].split(":")[0]: int(r["n"] or 0) for r in rows if r["n"]}})
    return out


# --------------------------------------------------------------------------- data capture

def _last_daily(ledger: Ledger) -> dict[str, Any] | None:
    rows = ledger.query("SELECT run_id, league, started_at, finished_at, status, summary_json FROM runs "
                        "WHERE command='daily' ORDER BY run_id DESC LIMIT 1")
    if not rows:
        return None
    r = rows[0]
    try:
        r["summary"] = json.loads(r.pop("summary_json") or "{}")
    except ValueError:
        r["summary"] = {}
    return r


def capture(ledger: Ledger, league: str, now: datetime | None = None) -> dict[str, Any]:
    now = now or datetime.now()
    eps = ledger.query("SELECT kind, status, COUNT(*) n FROM rec_episodes WHERE league=? GROUP BY 1, 2 ORDER BY 1, 2",
                       (league,))
    statuses = sorted({e["status"] for e in eps})
    kinds: dict[str, dict[str, int]] = {}
    for e in eps:
        kinds.setdefault(e["kind"], {})[e["status"]] = e["n"]
    tx = ledger.query("SELECT COUNT(*) n, SUM(is_me) mine, MAX(day) last, SUM(nhl_id IS NULL) unmatched "
                      "FROM transactions WHERE league=?", (league,))[0]
    dec = {r["origin"]: r["n"] for r in ledger.query(
        "SELECT origin, COUNT(*) n FROM decisions WHERE league=? GROUP BY 1", (league,))}
    proj = ledger.query("SELECT COUNT(DISTINCT as_of) days, MAX(as_of) last FROM projections WHERE league=?",
                        (league,))[0]
    unmatched_proj = total_proj = 0
    if proj["last"]:
        u = ledger.query("SELECT COUNT(*) n, SUM(nhl_id IS NULL) un FROM projections WHERE league=? AND as_of=?",
                         (league, proj["last"]))[0]
        total_proj, unmatched_proj = int(u["n"] or 0), int(u["un"] or 0)
    rp = ledger.query("SELECT COUNT(*) n, SUM(nhl_id IS NULL) un FROM rec_players WHERE league=?", (league,))[0]
    pulls = ledger.query("SELECT COUNT(*) pulled, SUM(n_players > 0) with_games, MIN(game_date) first, "
                         "MAX(game_date) last FROM realized_pulls")[0]
    lineups = ledger.query("SELECT COUNT(DISTINCT day) days FROM lineup_days WHERE league=?", (league,))[0]
    last = _last_daily(ledger)
    warnings: list[str] = []
    if last is None:
        warnings.append("No daily capture has run yet: schedule `fm harness daily` (docs/scheduling.md).")
    else:
        try:
            started = datetime.fromisoformat(str(last["started_at"]))
            age_h = (now - started).total_seconds() / 3600
            if age_h > STALE_RUN_HOURS:
                warnings.append(f"The last daily capture ran {age_h / 24:.1f} days ago: transaction feeds only keep "
                                "recent moves, so run it every day.")
        except ValueError:
            pass
        summ = last.get("summary") or {}
        for L in summ.get("leagues") or []:
            if L.get("league") != league:
                continue
            if L.get("skipped"):
                warnings.append(f"Last daily run skipped this league: {L['skipped']}")
            warnings.extend(str(w) for w in L.get("warnings") or [])
        if (summ.get("realized") or {}).get("error"):
            warnings.append(f"NHL results pull failed: {summ['realized']['error']}")
        if (summ.get("refit") or {}).get("error"):
            warnings.append(f"Refit failed: {summ['refit']['error']}")
        if last.get("status") not in (None, "ok"):
            warnings.append(f"Last daily run finished with status {last['status']}.")
    if pulls["first"]:
        first = date.fromisoformat(pulls["first"])
        yday = now.date() - timedelta(days=1)
        expected = (yday - first).days + 1
        missing = expected - int(pulls["pulled"] or 0)
        if missing > 0:
            warnings.append(f"{missing} game date(s) between {first} and {yday} were never pulled: "
                            "those windows cannot mature.")
    if total_proj and unmatched_proj / total_proj > 0.15:
        warnings.append(f"{unmatched_proj} of {total_proj} players in the latest snapshot have no NHL id "
                        "(`fm sync --review`).")
    return {
        "episodes": {"statuses": statuses, "kinds": kinds, "total": sum(e["n"] for e in eps)},
        "transactions": {"n": int(tx["n"] or 0), "mine": int(tx["mine"] or 0),
                         "others": int(tx["n"] or 0) - int(tx["mine"] or 0), "last": tx["last"]},
        "decisions": dec,
        "projections": {"days": int(proj["days"] or 0), "last": proj["last"]},
        "lineup_days": int(lineups["days"] or 0),
        "realized": {"pulled": int(pulls["pulled"] or 0), "with_games": int(pulls["with_games"] or 0),
                     "first": pulls["first"], "last": pulls["last"]},
        "unmatched": {"projections": unmatched_proj, "projections_total": total_proj,
                      "transactions": int(tx["unmatched"] or 0), "rec_players": int(rp["un"] or 0),
                      "rec_players_total": int(rp["n"] or 0)},
        "last_daily": None if last is None else {k: last.get(k) for k in ("started_at", "finished_at", "status",
                                                                          "league")},
        "warnings": warnings,
    }


# --------------------------------------------------------------------------- params

def _metric(m: Mapping[str, Any], key: str) -> float | None:
    v = m.get(key)
    return float(v) if isinstance(v, (int, float)) else None


def changelog(store: Any) -> dict[str, Any]:
    """Params versions newest first, the active one and the rollback targets."""
    try:
        versions = store.versions()
        active = store.active_name()
    except Exception as e:  # noqa: BLE001 - a broken versions dir must not break the page
        return {"active": "packaged", "rows": [], "targets": [], "default_target": None,
                "error": f"{type(e).__name__}: {e}"}
    rows = []
    for v in reversed(versions):
        m = v.get("metrics") or {}
        events = v.get("changelog") or []
        rows.append({"version": v.get("version"), "parent": v.get("parent"), "created": v.get("created"),
                     "status": v.get("status"), "changed_keys": list(v.get("changed_keys") or []),
                     "holdout_before": _metric(m, "holdout_before"), "holdout_after": _metric(m, "holdout_after"),
                     "hist_before": _metric(m, "hist_before"), "hist_after": _metric(m, "hist_after"),
                     "n_live": m.get("n_live"), "hash": v.get("hash"), "applied_by": v.get("applied_by"),
                     "applied_at": v.get("applied_at"),
                     "last_event": events[-1] if events else None,
                     "events": [{k: e.get(k) for k in ("at", "event", "by", "note")} for e in events]})
    targets: list[str] = []
    default = None
    if active != "packaged":
        cur = next((v for v in versions if v.get("version") == active), {})
        default = cur.get("parent") or "packaged"
        targets = [default] + [v["version"] for v in reversed(versions)
                               if v.get("status") in ROLLBACK_FROM and v["version"] not in (active, default)]
        if "packaged" not in targets:
            targets.append("packaged")
    return {"active": active, "rows": rows, "targets": targets, "default_target": default}


def calendar(today: date | None = None, data_dir: str | Path | None = None) -> dict[str, Any]:
    from ..prefs import harness_auto_apply
    from .refit import (AUTO_APPLY_START, REFIT_START, TIER_B_APPLY_START, is_refit_day, next_refit_day)

    today = today or date.today()
    try:
        pref = harness_auto_apply(data_dir)
    except Exception:  # noqa: BLE001
        pref = True
    return {"today": today.isoformat(), "locked": today < REFIT_START, "refit_start": REFIT_START.isoformat(),
            "next_refit": (today if is_refit_day(today) else next_refit_day(today)).isoformat(),
            "is_refit_day": is_refit_day(today), "auto_apply_start": AUTO_APPLY_START.isoformat(),
            "tier_b_apply_start": TIER_B_APPLY_START.isoformat(), "auto_apply_pref": pref,
            "auto_apply_active": bool(pref and today >= AUTO_APPLY_START)}


# --------------------------------------------------------------------------- the whole report

def full_report(ledger: Ledger, leagues: Iterable[str] | None = None, today: date | None = None,
                data_dir: str | Path | None = None) -> dict[str, Any]:
    """``metrics.status_report`` (unchanged keys) plus, per league, ``series`` / ``scorecard`` /
    ``capture`` and, top level, ``changelog`` and ``calendar``."""
    from .params_store import ParamsStore

    rep = status_report(ledger, leagues)
    for lg, L in rep["leagues"].items():
        extend_league(ledger, lg, L)
    rep["changelog"] = changelog(ParamsStore(ledger=ledger))
    rep["calendar"] = calendar(today, data_dir if data_dir is not None else ledger.data_dir)
    return rep


def extend_league(ledger: Ledger, league: str, L: dict[str, Any] | None = None) -> dict[str, Any]:
    """Add the M4 keys to one league's report (built when not given)."""
    L = L if L is not None else league_report(ledger, league)
    _, latest = latest_rows(ledger, league)
    L["series"] = series(ledger, league)
    L["scorecard"] = scorecard(latest)
    L["capture"] = capture(ledger, league)
    return L


# --------------------------------------------------------------------------- digest block

def _arrow(cur: float | None, prev: float | None) -> str:
    if cur is None or prev is None or prev == 0:
        return "→"
    change = (cur - prev) / abs(prev)
    if abs(change) < TREND_TOLERANCE:
        return "→"
    return "↓" if change < 0 else "↑"       # MAE: down is better


def health_block(ledger: Ledger, league: str) -> list[str] | None:
    """Three lines (projection MAE trend, hit rate, params version) for the digest, or None while
    nothing is trustworthy (the same rule as ``metrics.headline``)."""
    from .metrics import POOL_LABEL, headline, params_info

    if headline(ledger, league) is None:
        return None
    week, latest = latest_rows(ledger, league)
    prev_week = ledger.query("SELECT MAX(week) w FROM metric_snapshots WHERE league=? AND week < ?",
                             (league, week))[0]["w"]
    prev = {r["pool"]: r["value"] for r in ledger.query(
        "SELECT pool, value FROM metric_snapshots WHERE league=? AND week=? AND metric='proj_fpg_mae'",
        (league, prev_week))} if prev_week else {}
    mae_bits = []
    for r in latest:
        if r["metric"] == "proj_fpg_mae" and r["trust"] != "hidden" and r["value"] is not None:
            p = prev.get(r["pool"])
            was = f" (was {p:.2f})" if p is not None else ""
            mae_bits.append(f"{POOL_LABEL.get(r['pool'], r['pool'])} {r['value']:.2f} {_arrow(r['value'], p)}{was}")
    line1 = "Projection MAE (FPG, next 28 days): " + ("; ".join(mae_bits) if mae_bits else "not judgeable yet")
    hit_bits = []
    for kind, s in _hit_series(history_rows(ledger, league, "hit_rate")).items():
        if s["trust"] != "hidden" and s["value"] is not None:
            hit_bits.append(f"{kind} {s['value'] * 100:.0f}% (n={s['n']}, {s['trust']})")
    line2 = "Hit rate of the model's recs: " + ("; ".join(hit_bits) if hit_bits else "not judgeable yet")
    pi = params_info(ledger)
    h = (pi.get("hash") or "")[:10]
    line3 = f"Params: {pi.get('version')}" + (f" (hash {h})" if h else "") + f", {pi.get('source')}"
    return [line1, line2, line3]


__all__ = ["calendar", "capture", "changelog", "extend_league", "full_report", "health_block", "history_rows",
           "scorecard", "series"]
