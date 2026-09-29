"""The bar: weekly accuracy / recommendation-quality snapshots with trust labels (M2).

``grade_week(ledger, league, week_monday)`` computes the running bar as of a Monday (realized
results through the Sunday before) and replaces that week's ``metric_snapshots`` rows:

* Projection accuracy (``proj_fpg_mae``, ``proj_fpg_skill``, ``proj_week_mae`` per pool F / D / G).
  One projection snapshot per ISO week (its Monday, else the week's earliest day). Predicted
  ``fpg`` vs realized FPG over the next 28 days (players with >= ``MIN_GP_28`` GP), and
  predicted ``proj_week`` vs realized points over the next 7 days, split into a rate error
  ((fpg_pred - fpg_real) * gp_real) and an availability error (fpg_pred * (games_pred - gp_real)).
  Only snapshots whose window ended before ``week_monday`` and was fully pulled count; the bar
  pools every matured snapshot. Baselines from the archived inputs, each on the players it
  covers: season-to-date FPG, the frozen preseason fm (earliest snapshot), the provider
  projection and last season (the history baseline). Headline: skill = 1 - MAE_fm / MAE_to_date
  with a 95% paired bootstrap CI clustered by player (``BOOTSTRAP_N`` resamples, seeded).
* Recommendation quality (``hit_rate`` per ``kind:origin``): complete outcomes on each kind's
  primary window (lineup / week_pts recs 7d, others 28d), hit rate with a Wilson 95% CI and the
  mean realized gain; episodes graded from ``first_seen``, my own moves from their day.
* ``calibration``: predicted gain in points over the window (``predicted_pts``) vs realized, by
  decile (terciles when n < 100), waiver / injury / lineup recs.
* ``counterfactual``: my user-only waiver moves vs the model's pick of that day over the same
  window, plus how often the model's own projection agreed with my move.
* ``trade_cases``: trades are never aggregated, only listed.

Trust labels (plan section 2): projection MAE hidden < 150 player-windows per pool,
provisional >= 150, reliable >= 400 and >= 4 weekly snapshots; goalies never above
provisional before January 1. Hit rates hidden < 20, provisional 20-49, reliable >= 50.
Calibration hidden < 100. Trades: ``cases``.
"""
from __future__ import annotations

import hashlib
import json
import math
import random
from dataclasses import dataclass, field
from datetime import date, timedelta
from typing import Any, Iterable, Mapping, Sequence

from ..backtest.evaluate import metrics as _metrics
from ..backtest.evaluate import spearman
from .ledger import Ledger, now_iso
from .outcomes import (FLAG_KINDS, Realized, _fpg_of, days_between, load_players, points_scorer,
                       scoring_config)

POOLS = ("F", "D", "G")
POOL_LABEL = {"F": "forwards", "D": "defense", "G": "goalies"}
MIN_GP_28 = 8
PROJ_PROVISIONAL, PROJ_RELIABLE, PROJ_RELIABLE_WEEKS = 150, 400, 4
HIT_PROVISIONAL, HIT_RELIABLE = 20, 50
CALIBRATION_MIN = 100
DECILE_MIN = 100
BOOTSTRAP_N = 200
BOOTSTRAP_SEED = 20260928
Z95 = 1.959964
BASELINES = ("to_date", "preseason", "provider", "last_season")
HIT_KINDS = ("waiver", "lineup", "injury", "sell_high", "buy_low")
HIT_ORIGINS = ("followed", "partial", "ignored", "user_only")
CORE_HIT_ROWS = ("waiver:followed", "waiver:ignored", "lineup:followed", "lineup:ignored", "waiver:user_only")
CALIBRATION_KINDS = ("waiver", "injury", "lineup")
TRUST_ORDER = {"hidden": 0, "cases": 0, "provisional": 1, "reliable": 2}


# --------------------------------------------------------------------------- statistics

def wilson(k: int, n: int, z: float = Z95) -> tuple[float | None, float | None]:
    """Wilson score interval for k successes out of n (None, None when n == 0)."""
    if n <= 0:
        return None, None
    p = k / n
    denom = 1 + z * z / n
    centre = (p + z * z / (2 * n)) / denom
    half = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / denom
    return max(0.0, centre - half), min(1.0, centre + half)


def percentile(xs: Sequence[float], q: float) -> float | None:
    """Linear-interpolated percentile (q in 0..100) of ``xs``."""
    if not xs:
        return None
    s = sorted(xs)
    pos = (len(s) - 1) * q / 100.0
    lo = int(math.floor(pos))
    hi = min(lo + 1, len(s) - 1)
    return s[lo] + (s[hi] - s[lo]) * (pos - lo)


def skill_score(fm_abs: Sequence[float], base_abs: Sequence[float]) -> float | None:
    """1 - MAE_fm / MAE_base on paired absolute errors."""
    sb = sum(base_abs)
    return None if not fm_abs or sb <= 0 else 1.0 - sum(fm_abs) / sb


def bootstrap_skill(pairs: Iterable[tuple[Any, float, float]], n_boot: int = BOOTSTRAP_N,
                    seed: int = BOOTSTRAP_SEED, level: float = 0.95) -> tuple[float | None, float | None]:
    """Paired bootstrap CI of the skill score, clustered by player: ``pairs`` are
    (player, |err_fm|, |err_base|); whole players are resampled with replacement."""
    by: dict[Any, list[float]] = {}
    for pid, a, b in pairs:
        acc = by.setdefault(pid, [0.0, 0.0])
        acc[0] += a
        acc[1] += b
    keys = list(by)
    if len(keys) < 2:
        return None, None
    rng = random.Random(seed)
    stats = []
    for _ in range(n_boot):
        sf = sb = 0.0
        for _ in range(len(keys)):
            a, b = by[keys[rng.randrange(len(keys))]]
            sf += a
            sb += b
        if sb > 0:
            stats.append(1.0 - sf / sb)
    tail = (1 - level) / 2 * 100
    return percentile(stats, tail), percentile(stats, 100 - tail)


def calibration_bins(pred: Sequence[float], real: Sequence[float]) -> list[dict[str, Any]]:
    """Predicted vs realized by decile of predicted (terciles when n < ``DECILE_MIN``)."""
    n = len(pred)
    if n < 3:
        return []
    k = 10 if n >= DECILE_MIN else 3
    order = sorted(range(n), key=lambda i: pred[i])
    out = []
    for b in range(k):
        idx = order[b * n // k:(b + 1) * n // k]
        if not idx:
            continue
        ps, rs = [pred[i] for i in idx], [real[i] for i in idx]
        out.append({"bin": b + 1, "n": len(idx), "pred_lo": round(min(ps), 3), "pred_hi": round(max(ps), 3),
                    "mean_pred": round(sum(ps) / len(ps), 3), "mean_real": round(sum(rs) / len(rs), 3),
                    "hit_rate": round(sum(r > 0 for r in rs) / len(rs), 3)})
    return out


# --------------------------------------------------------------------------- trust labels

def season_start_year(day: date) -> int:
    return day.year if day.month >= 9 else day.year - 1


def projection_trust(n: int, weeks: int, pool: str, week: date) -> str:
    if n < PROJ_PROVISIONAL:
        return "hidden"
    label = "reliable" if n >= PROJ_RELIABLE and weeks >= PROJ_RELIABLE_WEEKS else "provisional"
    if pool == "G" and week < date(season_start_year(week) + 1, 1, 1):
        label = "provisional"
    return label


def hit_trust(n: int) -> str:
    if n < HIT_PROVISIONAL:
        return "hidden"
    return "reliable" if n >= HIT_RELIABLE else "provisional"


def calibration_trust(n: int) -> str:
    return "hidden" if n < CALIBRATION_MIN else "reliable"


def need_for(metric: str, n: int) -> int | None:
    """Observations still needed before ``metric`` leaves ``hidden``."""
    target = {"proj_fpg_mae": PROJ_PROVISIONAL, "proj_fpg_skill": PROJ_PROVISIONAL,
              "proj_week_mae": PROJ_PROVISIONAL, "hit_rate": HIT_PROVISIONAL, "counterfactual": HIT_PROVISIONAL,
              "calibration": CALIBRATION_MIN}.get(metric)
    return None if target is None else max(0, target - n)


# --------------------------------------------------------------------------- projection accuracy

def pool_of(positions: str | None) -> str:
    ps = {p.strip().upper() for p in (positions or "").split(",") if p.strip()}
    if ps == {"G"}:
        return "G"
    if "D" in ps and not ps & {"C", "LW", "RW", "F", "W"}:
        return "D"
    return "F"


def week_monday(day: date) -> date:
    return day - timedelta(days=day.weekday())


def weekly_snapshots(days: Iterable[str]) -> list[tuple[date, date]]:
    """(ISO-week Monday, snapshot day): the Monday itself, else the week's earliest day."""
    by: dict[date, date] = {}
    for s in days:
        d = date.fromisoformat(s)
        m = week_monday(d)
        if m not in by or d < by[m]:
            by[m] = d
    return sorted(by.items())


@dataclass
class ProjObs:
    week: date
    player: int
    pool: str
    pred: float
    real: float
    gp: int
    baselines: dict[str, float] = field(default_factory=dict)


@dataclass
class WeekObs:
    week: date
    player: int
    pool: str
    pred: float
    real: float
    rate_err: float
    avail_err: float


def _inputs(row: Mapping[str, Any]) -> dict[str, Any]:
    try:
        return json.loads(row.get("inputs_json") or "null") or {}
    except ValueError:
        return {}


def baseline_values(row: Mapping[str, Any], sc: Any, preseason: Mapping[str, Any]) -> dict[str, float]:
    """The baselines available for one archived projection row."""
    inp = _inputs(row)
    out: dict[str, float] = {}
    td = _fpg_of(inp.get("season"), sc)
    if td is not None:
        out["to_date"] = td
    pre = preseason.get(row["cid"])
    if pre is not None and pre.get("fpg") is not None:
        out["preseason"] = float(pre["fpg"])
    prov = inp.get("projection")
    if not prov and row.get("projected_json"):
        try:
            prov = json.loads(row["projected_json"])
        except ValueError:
            prov = None
    pv = _fpg_of(prov, sc)
    if pv is not None:
        out["provider"] = pv
    hist = inp.get("history") or {}
    if hist.get("rates"):
        out["last_season"] = float(sc.value(dict(hist["rates"])))
    return out


def projection_observations(ledger: Ledger, league: str, sc: Any, realized: Realized, week: date
                            ) -> tuple[list[ProjObs], list[WeekObs], dict[str, Any]]:
    """Every matured (snapshot, player) window as of ``week`` (a Monday)."""
    days = [r["as_of"] for r in ledger.query("SELECT DISTINCT as_of FROM projections WHERE league=? ORDER BY as_of",
                                             (league,))]
    info: dict[str, Any] = {"snapshots": len(weekly_snapshots(days)), "matured_28d": 0, "matured_7d": 0,
                            "unpulled": 0, "unmatched": 0}
    if not days:
        return [], [], info
    cols = "cid, nhl_id, positions, fpg, fpg_week, proj_week, inputs_json, projected_json"
    preseason = {r["cid"]: r for r in ledger.query(f"SELECT {cols} FROM projections WHERE league=? AND as_of=?",
                                                   (league, days[0]))}
    last_day = week - timedelta(days=1)
    fpg_obs: list[ProjObs] = []
    wk_obs: list[WeekObs] = []
    for monday, snap in weekly_snapshots(days):
        d28 = days_between(snap + timedelta(days=1), snap + timedelta(days=28))
        d7 = d28[:7]
        ok28 = d28[-1] <= last_day and realized.covered(d28)
        ok7 = d7[-1] <= last_day and realized.covered(d7)
        if (d28[-1] <= last_day and not ok28) or (d7[-1] <= last_day and not ok7):
            info["unpulled"] += 1
        if not (ok28 or ok7):
            continue
        info["matured_28d"] += ok28
        info["matured_7d"] += ok7
        for r in ledger.query(f"SELECT {cols} FROM projections WHERE league=? AND as_of=?",
                              (league, snap.isoformat())):
            if r["nhl_id"] is None:
                info["unmatched"] += 1
                continue
            pool = pool_of(r["positions"])
            pid = int(r["nhl_id"])
            if ok28 and r["fpg"] is not None:
                gp, pts = realized.window(pid, d28)
                if gp >= MIN_GP_28:
                    fpg_obs.append(ProjObs(monday, pid, pool, float(r["fpg"]), pts / gp, gp,
                                           baseline_values(r, sc, preseason)))
            if ok7 and r["proj_week"] is not None and float(r["proj_week"]) > 0:
                pred = float(r["proj_week"])
                pf = r["fpg_week"] if r["fpg_week"] is not None else r["fpg"]
                pf = float(pf) if pf is not None else 0.0
                gp, pts = realized.window(pid, d7)
                games_pred = pred / pf if pf > 0 else 0.0
                real_fpg = pts / gp if gp else 0.0
                rate = (pf - real_fpg) * gp
                wk_obs.append(WeekObs(monday, pid, pool, pred, pts, rate, pred - pts - rate))
    return fpg_obs, wk_obs, info


def _snap(week: date, league: str, metric: str, pool: str, value: float | None, n: int,
          lo: float | None = None, hi: float | None = None, trust: str = "hidden",
          detail: Mapping[str, Any] | None = None) -> dict[str, Any]:
    sid = hashlib.sha1(f"{week.isoformat()}|{league}|{metric}|{pool}".encode("utf-8")).hexdigest()[:20]
    rnd = (lambda v: None if v is None or (isinstance(v, float) and math.isnan(v)) else round(float(v), 4))
    return {"snapshot_id": sid, "week": week.isoformat(), "league": league, "metric": metric, "pool": pool,
            "value": rnd(value), "n": int(n), "ci_lo": rnd(lo), "ci_hi": rnd(hi), "trust": trust,
            "detail_json": json.dumps(dict(detail or {}), sort_keys=True, default=str), "created_at": now_iso()}


def _nan(v: float | None) -> float | None:
    return None if v is None or (isinstance(v, float) and math.isnan(v)) else v


def projection_rows(fpg_obs: Sequence[ProjObs], wk_obs: Sequence[WeekObs], week: date, league: str
                    ) -> list[dict[str, Any]]:
    rows = []
    for pool in POOLS:
        obs = [o for o in fpg_obs if o.pool == pool]
        weeks = len({o.week for o in obs})
        trust = projection_trust(len(obs), weeks, pool, week)
        m = _metrics([o.pred for o in obs], [o.real for o in obs], top=False)
        base: dict[str, Any] = {}
        for b in BASELINES:
            sub = [o for o in obs if b in o.baselines]
            if not sub:
                base[b] = {"n": 0}
                continue
            mb = _metrics([o.baselines[b] for o in sub], [o.real for o in sub], top=False)
            mf = _metrics([o.pred for o in sub], [o.real for o in sub], top=False)
            base[b] = {"n": len(sub), "mae": _nan(mb.mae), "spearman": _nan(mb.spearman), "fm_mae": _nan(mf.mae),
                       "skill": skill_score([abs(o.pred - o.real) for o in sub],
                                            [abs(o.baselines[b] - o.real) for o in sub])}
        per_week = []
        for wk in sorted({o.week for o in obs}):
            ws = [o for o in obs if o.week == wk]
            per_week.append({"week": wk.isoformat(), "n": len(ws),
                             "mae": round(sum(abs(o.pred - o.real) for o in ws) / len(ws), 4)})
        rows.append(_snap(week, league, "proj_fpg_mae", pool, _nan(m.mae), len(obs), trust=trust, detail={
            "bias": _nan(m.bias), "rmse": _nan(m.rmse), "spearman": _nan(m.spearman), "weeks": weeks,
            "per_week": per_week, "baselines": base, "min_gp": MIN_GP_28, "need": need_for("proj_fpg_mae", len(obs))}))
        paired = [(o.player, abs(o.pred - o.real), abs(o.baselines["to_date"] - o.real))
                  for o in obs if "to_date" in o.baselines]
        sk = skill_score([p[1] for p in paired], [p[2] for p in paired])
        lo, hi = bootstrap_skill(paired) if paired else (None, None)
        rows.append(_snap(week, league, "proj_fpg_skill", pool, sk, len(paired), lo, hi,
                          projection_trust(len(paired), len({o.week for o in obs if "to_date" in o.baselines}),
                                           pool, week),
                          {"vs": "to_date", "players": len({p[0] for p in paired}), "bootstrap": BOOTSTRAP_N,
                           "need": need_for("proj_fpg_skill", len(paired))}))
        wo = [o for o in wk_obs if o.pool == pool]
        mw = _metrics([o.pred for o in wo], [o.real for o in wo], top=False)
        wweeks = len({o.week for o in wo})
        rows.append(_snap(week, league, "proj_week_mae", pool, _nan(mw.mae), len(wo),
                          trust=projection_trust(len(wo), wweeks, pool, week), detail={
                              "bias": _nan(mw.bias), "spearman": _nan(mw.spearman), "weeks": wweeks,
                              "rate_mae": (sum(abs(o.rate_err) for o in wo) / len(wo)) if wo else None,
                              "avail_mae": (sum(abs(o.avail_err) for o in wo) / len(wo)) if wo else None,
                              "need": need_for("proj_week_mae", len(wo))}))
    return rows


# --------------------------------------------------------------------------- recommendation quality

def primary_window(kind: str, units: str | None) -> str:
    if kind == "lineup" or units == "week_pts":
        return "7d"
    return "28d"


def matured_outcomes(ledger: Ledger, league: str, week: date) -> list[dict[str, Any]]:
    """Complete outcomes whose window ended before ``week`` on each subject's primary window."""
    rows = ledger.query("SELECT * FROM outcomes WHERE league=? AND complete=1 AND window_end < ?",
                        (league, week.isoformat()))
    out = []
    for r in rows:
        units = r["gain_units"] if r["origin"] != "user_only" else None
        if r["window"] != primary_window(r["kind"], units):
            continue
        if r["basis"] == "acted_on":
            continue
        r["detail"] = json.loads(r["detail_json"] or "{}")
        out.append(r)
    return out


def hit_rows(outs: Sequence[Mapping[str, Any]], week: date, league: str) -> list[dict[str, Any]]:
    groups: dict[str, list[Mapping[str, Any]]] = {k: [] for k in CORE_HIT_ROWS}
    for o in outs:
        if o["kind"] == "trade" or o["origin"] not in HIT_ORIGINS or o["hit"] is None:
            continue
        if o["kind"] not in HIT_KINDS:
            continue
        groups.setdefault(f"{o['kind']}:{o['origin']}", []).append(o)
    rows = []
    for key, os_ in sorted(groups.items()):
        n = len(os_)
        k = sum(int(o["hit"]) for o in os_)
        lo, hi = wilson(k, n)
        kind = key.split(":")[0]
        gains = [float(o["realized_gain"]) for o in os_ if o["realized_gain"] is not None]
        rows.append(_snap(week, league, "hit_rate", key, k / n if n else None, n, lo, hi, hit_trust(n), {
            "hits": k, "mean_gain": (sum(gains) / len(gains)) if gains else None,
            "units": "fpg change" if kind in FLAG_KINDS else "pts",
            "windows": sorted({o["window"] for o in os_}), "need": need_for("hit_rate", n)}))
    return rows


def calibration_row(outs: Sequence[Mapping[str, Any]], week: date, league: str) -> dict[str, Any]:
    sel = [o for o in outs if o["kind"] in CALIBRATION_KINDS and o["origin"] != "user_only"
           and o["predicted_pts"] is not None and o["realized_gain"] is not None]
    pred = [float(o["predicted_pts"]) for o in sel]
    real = [float(o["realized_gain"]) for o in sel]
    n = len(sel)
    ratio = (sum(real) / sum(pred)) if pred and sum(pred) > 0 else None
    return _snap(week, league, "calibration", "all", ratio, n, trust=calibration_trust(n), detail={
        "bins": calibration_bins(pred, real), "binning": "decile" if n >= DECILE_MIN else "tercile",
        "by_kind": {k: sum(o["kind"] == k for o in sel) for k in CALIBRATION_KINDS},
        "spearman": _nan(spearman(pred, real)) if n >= 3 else None, "value": "realized / predicted",
        "need": need_for("calibration", n)})


def counterfactual_row(outs: Sequence[Mapping[str, Any]], week: date, league: str) -> dict[str, Any]:
    mine = [o for o in outs if o["origin"] == "user_only" and o["kind"] == "waiver" and o["realized_gain"] is not None]
    paired = [(float(o["realized_gain"]), float(o["detail"]["alt"]["gain"])) for o in mine
              if (o["detail"].get("alt") or {}).get("gain") is not None]
    agreed = [o["detail"].get("model_fpg_delta") for o in mine if o["detail"].get("model_fpg_delta") is not None]
    n = len(paired)
    my_mean = (sum(p[0] for p in paired) / n) if n else None
    model_mean = (sum(p[1] for p in paired) / n) if n else None
    return _snap(week, league, "counterfactual", "user_only",
                 (my_mean - model_mean) if n else None, n, trust=hit_trust(n), detail={
                     "moves": len(mine),
                     "my_mean_all": (sum(float(o["realized_gain"]) for o in mine) / len(mine)) if mine else None,
                     "my_mean": my_mean, "model_mean": model_mean,
                     "model_better": (sum(b > a for a, b in paired) / n) if n else None,
                     "model_agreed": (sum(d > 0 for d in agreed) / len(agreed)) if agreed else None,
                     "value": "my mean gain - the model's pick's mean gain (pts, same windows)",
                     "need": need_for("counterfactual", n)})


def trade_cases_row(ledger: Ledger, league: str, week: date) -> dict[str, Any]:
    rows = ledger.query("SELECT * FROM outcomes WHERE league=? AND kind='trade' AND window IN ('28d','ros')"
                        " AND origin IN ('followed','partial','proposed','user_only') AND window_start < ?"
                        " ORDER BY window_start", (league, week.isoformat()))
    cases = []
    for r in rows:
        d = json.loads(r["detail_json"] or "{}")
        cases.append({"title": d.get("title"), "origin": r["origin"], "basis": r["basis"], "day": d.get("day"),
                      "window": r["window"], "realized_gain": r["realized_gain"], "complete": bool(r["complete"]),
                      "label": d.get("label") or ("complete" if r["complete"] else "partial"),
                      "predicted_gain": r["predicted_gain"], "gain_units": r["gain_units"]})
    return _snap(week, league, "trade_cases", "trade", None, len(cases), trust="cases", detail={"cases": cases})


# --------------------------------------------------------------------------- weekly grade

def compute_week(ledger: Ledger, league: str, week: date) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    """The bar for ``league`` as of Monday ``week`` (rows not written) and some bookkeeping."""
    week = week_monday(week)
    cfg, src = scoring_config(ledger, league)
    sc = points_scorer(cfg)
    info: dict[str, Any] = {"league": league, "week": week.isoformat(), "scoring": src}
    if sc is None:
        info["note"] = "not a points league (or no scoring known): not graded"
        return [], info
    realized = Realized(ledger, sc, None, week)
    fpg_obs, wk_obs, pinfo = projection_observations(ledger, league, sc, realized, week)
    info.update(pinfo)
    rows = projection_rows(fpg_obs, wk_obs, week, league)
    outs = matured_outcomes(ledger, league, week)
    rows += hit_rows(outs, week, league)
    rows.append(calibration_row(outs, week, league))
    rows.append(counterfactual_row(outs, week, league))
    rows.append(trade_cases_row(ledger, league, week))
    info["outcomes_used"] = len(outs)
    return rows, info


def grade_week(ledger: Ledger, league: str, week_monday_: date) -> dict[str, Any]:
    """Compute and store (replace) the ``metric_snapshots`` rows of ``league`` for that week."""
    rows, info = compute_week(ledger, league, week_monday_)
    ledger.execute("DELETE FROM metric_snapshots WHERE league=? AND week=?", (league, info["week"]))
    ledger.commit()
    ledger.upsert("metric_snapshots", rows, ("snapshot_id",))
    info["rows"] = len(rows)
    try:  # champion / challenger: shadow-score the version a promoted params version replaced
        from .refit import shadow_check

        sh = shadow_check(ledger, week_monday(week_monday_))
        if sh is not None:
            info["shadow"] = sh
    except Exception as e:  # noqa: BLE001 - never fail grading over shadow scoring
        info["shadow_error"] = f"{type(e).__name__}: {e}"
    info["trust"] = {t: sum(r["trust"] == t for r in rows) for t in ("hidden", "provisional", "reliable", "cases")}
    return info


# --------------------------------------------------------------------------- reading the bar

def _decode(r: Mapping[str, Any]) -> dict[str, Any]:
    out = dict(r)
    out["detail"] = json.loads(out.pop("detail_json", None) or "{}")
    return out


def latest_rows(ledger: Ledger, league: str) -> tuple[str | None, list[dict[str, Any]]]:
    wk = ledger.query("SELECT MAX(week) w FROM metric_snapshots WHERE league=?", (league,))[0]["w"]
    if not wk:
        return None, []
    return wk, [_decode(r) for r in ledger.query(
        "SELECT * FROM metric_snapshots WHERE league=? AND week=? ORDER BY metric, pool", (league, wk))]


def _pct(v: float | None) -> str:
    return "-" if v is None else f"{v * 100:.0f}%"


def headline(ledger: Ledger, league: str) -> str | None:
    """One line for the digest, or None while nothing is trustworthy (every metric hidden)."""
    _, rows = latest_rows(ledger, league)
    bits = []
    for r in rows:
        if r["trust"] not in ("provisional", "reliable") or r["value"] is None:
            continue
        if r["metric"] == "proj_fpg_skill":
            word = "beat" if r["value"] >= 0 else "trail"
            ci = f", 95% CI {_pct(r['ci_lo'])} to {_pct(r['ci_hi'])}" if r["ci_lo"] is not None else ""
            bits.append(f"{POOL_LABEL.get(r['pool'], r['pool'])} projections {word} season-to-date by "
                        f"{abs(r['value']) * 100:.0f}%{ci} ({r['trust']}, n={r['n']})")
        elif r["metric"] == "hit_rate" and r["pool"].endswith(":followed"):
            kind = r["pool"].split(":")[0]
            bits.append(f"{kind} recs followed hit {_pct(r['value'])} "
                        f"(95% CI {_pct(r['ci_lo'])}-{_pct(r['ci_hi'])}, {r['trust']}, n={r['n']})")
    return ("Model: " + "; ".join(bits)) if bits else None


def not_judgeable(rows: Sequence[Mapping[str, Any]]) -> list[dict[str, Any]]:
    out = []
    for r in rows:
        if r["trust"] != "hidden":
            continue
        need = (r.get("detail") or {}).get("need")
        out.append({"metric": r["metric"], "pool": r["pool"], "n": r["n"], "need": need})
    return out


def default_hidden() -> list[dict[str, Any]]:
    """What is not judgeable before any grading ran."""
    rows = [{"metric": m, "pool": p, "n": 0, "need": need_for(m, 0)}
            for m in ("proj_fpg_mae", "proj_week_mae") for p in POOLS]
    rows += [{"metric": "hit_rate", "pool": k, "n": 0, "need": need_for("hit_rate", 0)} for k in CORE_HIT_ROWS]
    rows += [{"metric": "calibration", "pool": "all", "n": 0, "need": need_for("calibration", 0)},
             {"metric": "counterfactual", "pool": "user_only", "n": 0, "need": need_for("counterfactual", 0)}]
    return rows


def params_info(ledger: Ledger) -> dict[str, Any]:
    try:
        from ..valuation.params import params_hash, source
        h, src = params_hash(), source()
    except Exception as e:  # noqa: BLE001
        h, src = None, f"unavailable ({type(e).__name__})"
    try:
        from .params_store import ParamsStore

        st = ParamsStore(ledger=ledger)
        version, n = st.active_name(), len(st.versions())
    except Exception:  # noqa: BLE001
        active = ledger.query("SELECT version FROM param_versions WHERE status='active' LIMIT 1")
        version, n = (active[0]["version"] if active else "packaged"), ledger.count("param_versions")
    return {"version": version, "hash": h, "source": src, "versions": n}


def league_report(ledger: Ledger, league: str) -> dict[str, Any]:
    week, rows = latest_rows(ledger, league)
    by = lambda m: [r for r in rows if r["metric"] == m]  # noqa: E731
    proj = [r for r in rows if r["metric"].startswith("proj_")]
    cal = by("calibration")
    cf = by("counterfactual")
    trades = by("trade_cases")
    oc = ledger.query("SELECT COUNT(*) n, SUM(complete) complete, SUM(realized_gain IS NULL) ungradable,"
                      " SUM(detail_json LIKE '%\"unmatched\": [\"%') unmatched FROM outcomes WHERE league=?",
                      (league,))[0]
    return {
        "league": league, "week": week, "graded": week is not None,
        "projection": proj, "hit_rates": by("hit_rate"),
        "calibration": cal[0] if cal else None, "counterfactual": cf[0] if cf else None,
        "trades": (trades[0]["detail"].get("cases") if trades else []),
        "not_judgeable": not_judgeable(rows) if rows else default_hidden(),
        "outcomes": {k: int(oc[k] or 0) for k in ("n", "complete", "ungradable", "unmatched")},
        "headline": headline(ledger, league),
    }


def known_leagues(ledger: Ledger) -> list[str]:
    found: set[str] = set()
    for t in ("rec_episodes", "projections", "metric_snapshots", "decisions"):
        found |= {r["league"] for r in ledger.query(f"SELECT DISTINCT league FROM {t} WHERE league IS NOT NULL")}
    return sorted(found)


def status_report(ledger: Ledger, leagues: Iterable[str] | None = None) -> dict[str, Any]:
    """The bar for every league (latest stored snapshot), for ``fm harness status`` and /health."""
    lgs = list(leagues) if leagues else known_leagues(ledger)
    return {"params": params_info(ledger), "leagues": {lg: league_report(ledger, lg) for lg in lgs},
            "thresholds": {"projection": {"provisional": PROJ_PROVISIONAL, "reliable": PROJ_RELIABLE,
                                          "reliable_weeks": PROJ_RELIABLE_WEEKS,
                                          "goalies": "provisional at most before January 1"},
                           "hit_rate": {"provisional": HIT_PROVISIONAL, "reliable": HIT_RELIABLE},
                           "calibration": {"min": CALIBRATION_MIN}, "trades": "case list only"}}


def empty_report() -> dict[str, Any]:
    return {"params": None, "leagues": {}, "note": "no harness ledger yet: run `fm harness daily`"}


__all__ = ["BOOTSTRAP_N", "CALIBRATION_MIN", "HIT_PROVISIONAL", "HIT_RELIABLE", "PROJ_PROVISIONAL",
           "PROJ_RELIABLE", "bootstrap_skill", "calibration_bins", "calibration_trust", "compute_week",
           "default_hidden", "empty_report", "grade_week", "headline", "hit_trust", "latest_rows", "pool_of",
           "projection_trust", "skill_score", "status_report", "week_monday", "weekly_snapshots", "wilson"]
