"""View model for the /health page: chart specs (SVG from ``web.charts`` + a data-table
fallback) built from ``harness.health.series`` output, and small formatting helpers."""
from __future__ import annotations

from typing import Any, Mapping, Sequence

from . import charts
from .charts import DASHES, Line

POOL_TITLE = {"F": "Forwards", "D": "Defense", "G": "Goalies"}
BASELINE_LABEL = {"to_date": "Season to date", "preseason": "Preseason fm", "provider": "Provider projection",
                  "last_season": "Last season"}
BASELINES = tuple(BASELINE_LABEL)
KIND_TITLE = {"waiver": "Waiver", "lineup": "Lineup", "injury": "Injury", "sell_high": "Sell-high",
              "buy_low": "Buy-low"}
ORIGIN_LABEL = {"followed": "Followed", "partial": "Partly followed", "ignored": "Ignored",
                "user_only": "Your own moves"}
METRIC_LABEL = {"proj_fpg_mae": "Projection MAE (28-day FPG)", "proj_fpg_skill": "Skill vs season-to-date",
                "proj_week_mae": "Week points MAE", "hit_rate": "Hit rate", "calibration": "Calibration",
                "counterfactual": "Your moves vs the model's"}


def pct(v: Any) -> str:
    return "-" if v is None else f"{float(v) * 100:.0f}%"


def num(v: Any, nd: int = 2) -> str:
    return "-" if v is None else f"{float(v):.{nd}f}"


def signed_pct(v: Any) -> str:
    return "-" if v is None else f"{float(v) * 100:+.0f}%"


def _table(x: Sequence[str], lines: Sequence[Line], fmt, extra: Mapping[str, Sequence[Any]] | None = None
           ) -> dict[str, Any]:
    head = ["Week"] + [ln.label for ln in lines] + list(extra or {})
    rows = []
    for i, wk in enumerate(x):
        row = [wk] + [fmt(ln.values[i]) if i < len(ln.values) else "-" for ln in lines]
        row += [str(v[i]) if i < len(v) and v[i] is not None else "-" for v in (extra or {}).values()]
        rows.append(row)
    return {"head": head, "rows": rows}


def _chart(cid: str, s: Mapping[str, Any], lines: list[Line], title: str, desc: str, fmt, **kw: Any) -> dict[str, Any]:
    x = list(s.get("x") or [])
    visible = s.get("trust", "hidden") != "hidden" and bool(x)
    out = {"id": cid, "title": title, "trust": s.get("trust", "hidden"), "n": s.get("n", 0), "need": s.get("need"),
           "visible": visible, "points": len(x)}
    if visible:
        band = kw.pop("band", None)
        out["svg"] = charts.line_chart(cid, x, lines, title=title, desc=desc, y_fmt=fmt, band=band, **kw)
        out["legend"] = charts.legend(lines, "95% CI (Wilson)" if band else None)
        extra = {"95% CI": [f"{fmt(a)} to {fmt(b)}" if a is not None and b is not None else None
                            for a, b in zip(*band)]} if band else {}
        if s.get("n_by_week"):
            extra["N"] = list(s["n_by_week"])
        out["table"] = _table(x, lines, fmt, extra)
    return out


def _baseline_lines(values: Mapping[str, Sequence[float | None]], main_label: str, skip: Sequence[str] = ()
                    ) -> list[Line]:
    lines = [Line("fm", main_label, list(values.get("fm") or []), "main")]
    for i, b in enumerate(BASELINES):
        if b in skip or b not in values:
            continue
        vals = list(values[b])
        if any(v is not None for v in vals):
            lines.append(Line(b, BASELINE_LABEL[b], vals, "base", DASHES[i % len(DASHES)]))
    return lines


def health_charts(series: Mapping[str, Any] | None, league: str) -> dict[str, list[dict[str, Any]]]:
    """{"mae": [...per pool], "skill": [...per pool], "hit": [...per kind]} chart specs."""
    series = series or {}
    out: dict[str, list[dict[str, Any]]] = {"mae": [], "skill": [], "hit": []}
    for pool in ("F", "D", "G"):
        s = (series.get("mae") or {}).get(pool) or {}
        lines = _baseline_lines(s.get("lines") or {}, "fm model")
        latest = f"latest {num(s.get('value'))}" if s.get("value") is not None else "no value yet"
        out["mae"].append(_chart(
            f"{league}-mae-{pool}", s, lines, f"{POOL_TITLE[pool]}: projection MAE, next 28 days",
            f"Mean absolute error of projected fantasy points per game against the next 28 days, one point per "
            f"weekly snapshot; fm model in bold, baselines as thin lines; lower is better; {latest}.",
            lambda v: num(v)) | {"pool": pool, "label": POOL_TITLE[pool]})
        s = (series.get("skill") or {}).get(pool) or {}
        lines = _baseline_lines(s.get("lines") or {}, "fm model")
        out["skill"].append(_chart(
            f"{league}-skill-{pool}", s, lines, f"{POOL_TITLE[pool]}: skill vs season-to-date",
            "Skill score 1 - MAE / MAE of season-to-date FPG per weekly snapshot; above zero beats season-to-date; "
            f"season-to-date itself is the zero line; latest fm skill {pct(s.get('value'))}.",
            pct, zero_line=True) | {"pool": pool, "label": POOL_TITLE[pool]})
    for kind, s in (series.get("hit") or {}).items():
        band = ((s.get("band") or {}).get("lo") or [], (s.get("band") or {}).get("hi") or [])
        lines = [Line("rate", "Hit rate", list((s.get("lines") or {}).get("rate") or []), "main")]
        out["hit"].append(_chart(
            f"{league}-hit-{kind}", s, lines, f"{KIND_TITLE.get(kind, kind)} recommendations: hit rate",
            "Share of the model's graded recommendations of this kind (followed, partly followed or ignored) that "
            f"gained points, as of each graded week, with a Wilson 95% band; latest {pct(s.get('value'))} "
            f"of {s.get('n', 0)}.", pct, band=band, y_domain=(0.0, 1.0)) | {"kind": kind,
                                                                            "label": KIND_TITLE.get(kind, kind)})
    return out


def refit_rows(result: Mapping[str, Any] | None) -> list[dict[str, Any]]:
    """The dry-run proposal rows, formatted."""
    rows = []
    for r in (result or {}).get("rows") or []:
        rows.append({**r, "current_s": f"{r['current']:.4g}", "candidate_s": f"{r['candidate']:.4g}",
                     "holdout": f"{num(r.get('holdout_before'), 4)} → {num(r.get('holdout_after'), 4)}",
                     "hist": f"{num(r.get('hist_before'), 4)} → {num(r.get('hist_after'), 4)}"})
    return rows


__all__ = ["BASELINE_LABEL", "KIND_TITLE", "METRIC_LABEL", "ORIGIN_LABEL", "health_charts", "num", "pct",
           "refit_rows", "signed_pct"]
