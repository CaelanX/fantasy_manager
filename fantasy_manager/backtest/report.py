"""Markdown report for a backtest run."""
from __future__ import annotations

import math
from pathlib import Path
from typing import Any, Mapping, Sequence

from .data import season_label
from .evaluate import INSEASON_MODELS, ResultRow, head_to_head, summarize
from .fit import REPORT_AGES

PRESEASON_ORDER = ("naive", "marcel", "fm_current", "fm_fitted", "fm_multi")


def _f(v: Any, d: int = 3) -> str:
    if v is None or (isinstance(v, float) and math.isnan(v)):
        return "-"
    if isinstance(v, float):
        return f"{v:.{d}f}"
    return str(v)


def _pct(v: Any) -> str:
    return "-" if v is None or (isinstance(v, float) and math.isnan(v)) else f"{v:.0%}"


def _table(headers: Sequence[str], rows: Sequence[Sequence[Any]]) -> list[str]:
    out = ["| " + " | ".join(headers) + " |", "|" + "|".join("---" for _ in headers) + "|"]
    out += ["| " + " | ".join(str(c) for c in r) + " |" for r in rows]
    return out


def _ordered(models: Sequence[str], order: Sequence[str]) -> list[str]:
    return [m for m in order if m in models] + [m for m in models if m not in order]


def summary_table(rows: Sequence[ResultRow], kind: str, pool: str, order: Sequence[str]) -> list[str]:
    s = summarize(rows, kind, pool)
    if not s:
        return ["(no rows)"]
    body = []
    for m in _ordered(list(s), order):
        v = s[m]
        body.append([f"`{m}`", _f(v["mae"]), _f(v["rmse"]), _f(v["bias"], 3), _f(v["spearman"]),
                     _pct(v["top50"]), _pct(v["top100"]), int(v["n"])])
    return _table(["model", "MAE", "RMSE", "bias", "Spearman", "top-50", "top-100", "n (player-seasons)"], body)


def per_season_table(rows: Sequence[ResultRow], kind: str, pool: str, order: Sequence[str]) -> list[str]:
    sel = [r for r in rows if r.kind == kind and r.pool == pool]
    models = _ordered(sorted({r.model for r in sel}), order)
    keys = sorted({(r.season, r.checkpoint) for r in sel})
    idx = {(r.season, r.checkpoint, r.model): r for r in sel}
    headers = ["season" if kind == "preseason" else "checkpoint", "n"]
    headers += [f"{m} MAE" for m in models] + [f"{m} rho" for m in models]
    body = []
    for s, cp in keys:
        first = next((idx[(s, cp, m)] for m in models if (s, cp, m) in idx), None)
        label = season_label(s) if cp is None else f"{season_label(s)} {cp[5:]}"
        cells: list[Any] = [label, first.n if first else "-"]
        maes = {m: idx[(s, cp, m)].mae for m in models if (s, cp, m) in idx}
        best = min(maes.values()) if maes else None
        for m in models:
            v = maes.get(m)
            cells.append(f"**{_f(v)}**" if v is not None and v == best else _f(v))
        for m in models:
            r = idx.get((s, cp, m))
            cells.append(_f(r.spearman) if r else "-")
        body.append(cells)
    return _table(headers, body)


def verdict(rows: Sequence[ResultRow]) -> list[str]:
    s = summarize(rows, "preseason", "skaters")
    if "fm_current" not in s:
        return ["`fm_current` was not evaluated."]
    out = []
    cur = s["fm_current"]
    for other in ("naive", "marcel", "fm_fitted", "fm_multi"):
        if other not in s:
            continue
        o = s[other]
        w_mae, n = head_to_head(rows, "fm_current", other, metric="mae")
        w_rho, _ = head_to_head(rows, "fm_current", other, metric="spearman")
        better = cur["mae"] < o["mae"]
        rel = (o["mae"] - cur["mae"]) / o["mae"] if o["mae"] else 0.0
        out.append(f"- `fm_current` vs `{other}` (skaters): MAE {cur['mae']:.3f} vs {o['mae']:.3f} "
                   f"({'better' if better else 'worse'} by {abs(rel):.1%}); lower MAE in {w_mae}/{n} seasons, "
                   f"higher Spearman in {w_rho}/{n} (rho {cur['spearman']:.3f} vs {o['spearman']:.3f}).")
    g = summarize(rows, "preseason", "goalies")
    if "fm_current" in g:
        parts = ", ".join(f"`{m}` {g[m]['mae']:.3f}" for m in _ordered(list(g), PRESEASON_ORDER))
        out.append(f"- Goalies (MAE): {parts}.")
    ins = summarize(rows, "inseason", "skaters")
    if ins:
        parts = ", ".join(f"`{m}` {ins[m]['mae']:.3f}" for m in _ordered(list(ins), INSEASON_MODELS))
        out.append(f"- In-season rest-of-season skaters (MAE): {parts}.")
    return out


def fitted_section(fitted: Mapping[str, Any]) -> list[str]:
    out = [f"Fitted on scoring `{fitted.get('scoring')}` ({fitted.get('generated')}), "
           f"{fitted['preseason']['pairs']} season pairs."]
    pre = fitted["preseason"]
    app_k = pre.get("app_k") or {}
    if "F" in app_k:   # the app's 3-season baseline k per group
        out += ["", "**Preseason shrinkage k** toward the positional mean (1 season = `fm_fitted`, "
                "3 seasons = `fm_multi`, which the app uses):", ""]
        out += _table(["group", "fitted k (1 season)", "MAE at k=0", "MAE at fitted k", "fitted k (3 seasons)",
                       "MAE at fitted k (3 seasons)", "app k", "MAE at app k (3 seasons)"],
                      [[g, _f(pre["k"][g], 0), _f(pre["mae_by_k"][g].get("0"), 4), _f(pre["mae_at_fitted_k"][g], 4),
                        _f((pre.get("k_multi") or {}).get(g), 0), _f((pre.get("mae_at_fitted_k_multi") or {}).get(g), 4),
                        _f(app_k.get(g), 0), _f(pre["mae_at_app_k"][g], 4)] for g in ("F", "D", "G")])
    else:              # files written before the app switched to the 3-season baseline
        out += ["", "**Preseason shrinkage k** (prior season toward positional mean; app used "
                f"{app_k.get('skater')} skaters / {app_k.get('goalie')} goalies):", ""]
        out += _table(["group", "fitted k", "MAE at k=0", "MAE at app k", "MAE at fitted k"],
                      [[g, _f(pre["k"][g], 0), _f(pre["mae_by_k"][g].get("0"), 4),
                        _f(pre["mae_at_app_k"][g], 4), _f(pre["mae_at_fitted_k"][g], 4)] for g in ("F", "D", "G")])
    out.append(f"\nSkaters pooled: k = {_f(pre['k_skater_pooled'], 0)}.")
    if pre.get("k_multi"):
        km = pre["k_multi"]
        out.append(f"`fm_multi` (5/4/3-weighted seasons) k: F {km['F']:.0f}, D {km['D']:.0f}, G {km['G']:.0f} "
                   "(in games of the most recent season).")
    ins = fitted.get("inseason")
    if ins:
        w, aw = ins["recency_weights"], ins["app"]["recency_weights"]
        out += ["", f"**In-season** ({len(ins['seasons'])} seasons, {ins['n_skater_obs']} skater checkpoint obs): "
                f"k skater {ins['k_skater']:.0f} (app {ins['app']['k_skater']}), k goalie {ins['k_goalie']:.0f} "
                f"(app {ins['app']['k_goalie']}); recency weights season/L30/L15/L7 = "
                f"{w['season']:.2f}/{w['last30']:.2f}/{w['last15']:.2f}/{w['last7']:.2f} "
                f"(app {aw['season']:.2f}/{aw['last30']:.2f}/{aw['last15']:.2f}/{aw['last7']:.2f}). "
                f"In-sample MAE app {ins['mae_app']:.4f} -> fitted {ins['mae_fitted']:.4f} "
                f"(shrink only, fitted k: {ins['mae_shrink_only_fitted_k']:.4f})."]
    rep = fitted.get("age_curve_report") or {}
    if rep:
        out += ["", "**Age curve** (FPG level, peak = 1.00; 90% bootstrap interval; n = pairs at that age):", ""]
        body = []
        for g in ("F", "D", "G"):
            if g not in rep:
                continue
            peak = (fitted.get("age_curve") or {}).get(g, {}).get("peak_age")
            cells = [f"{g} (peak {peak})"]
            for a in REPORT_AGES:
                v = rep[g].get(str(a))
                cells.append("-" if not v else f"{v['level']:.2f} [{_f(v['lo'], 2)}-{_f(v['hi'], 2)}] n={v['n']}")
            body.append(cells)
        out += _table(["group"] + [str(a) for a in REPORT_AGES], body)
    return out


def recommendations(rows: Sequence[ResultRow], fitted: Mapping[str, Any] | None) -> list[str]:
    out: list[str] = []
    s = summarize(rows, "preseason", "skaters")
    g = summarize(rows, "preseason", "goalies")
    best = min(s, key=lambda m: s[m]["mae"]) if s else None
    if best and best != "fm_current" and "fm_current" in s:
        b, c = s[best], s["fm_current"]
        out.append(f"- Preseason baseline: `{best}` is the most accurate skater model (MAE {b['mae']:.3f} vs "
                   f"{c['mae']:.3f}, Spearman {b['spearman']:.3f} vs {c['spearman']:.3f}"
                   + (f"; goalies {g[best]['mae']:.3f} vs {g['fm_current']['mae']:.3f}" if best in g and "fm_current" in g else "")
                   + ").")
    if fitted:
        from ..valuation.blend import K_BASELINE, K_INSEASON, RECENCY_WEIGHTS
        from ..valuation.dynasty import PRODUCTION_CURVES

        pre = fitted["preseason"]
        km = pre.get("k_multi") or {}
        if km and any(abs(float(km[g]) - float(K_BASELINE[g])) > 0.5 for g in ("F", "D", "G") if g in km):
            out.append(f"- `blend.K_BASELINE` (3-season history toward the positional mean): data prefers "
                       f"F {km['F']:.0f} / D {km['D']:.0f} / G {km['G']:.0f} vs app "
                       f"{K_BASELINE['F']:g} / {K_BASELINE['D']:g} / {K_BASELINE['G']:g}.")
        ins = fitted.get("inseason")
        if ins:
            w = ins["recency_weights"]
            if abs(ins["k_skater"] - K_INSEASON["skater"]) > 0.5 or abs(ins["k_goalie"] - K_INSEASON["goalie"]) > 0.5:
                out.append(f"- `blend.K_INSEASON` (season-to-date toward baseline): data prefers {ins['k_skater']:.0f} "
                           f"skaters / {ins['k_goalie']:.0f} goalies vs app {K_INSEASON['skater']:g} / "
                           f"{K_INSEASON['goalie']:g}.")
            if any(abs(w[k] - RECENCY_WEIGHTS[k]) > 0.011 for k in w):
                a = RECENCY_WEIGHTS
                out.append(f"- `RECENCY_WEIGHTS`: season {w['season']:.2f} / L30 {w['last30']:.2f} / L15 "
                           f"{w['last15']:.2f} / L7 {w['last7']:.2f} vs app {a['season']:.2f} / {a['last30']:.2f} / "
                           f"{a['last15']:.2f} / {a['last7']:.2f}.")
        dyn = fitted.get("dynasty_age_curves") or {}
        stale = [g for g in ("F", "D") if g in dyn and
                 [tuple(map(float, pt)) for pt in dyn[g]] != [tuple(map(float, pt)) for pt in PRODUCTION_CURVES[g]]]
        if stale:
            out.append(f"- `dynasty.PRODUCTION_CURVES` ({', '.join(stale)}) differ from this fit's "
                       "`dynasty_age_curves`.")
        if len(out) == (1 if best and best != "fm_current" and "fm_current" in s else 0):
            out.append("- The app's constants (valuation/fitted_params.json) match this fit; nothing to change.")
        elif fitted.get("scoring", "espn") == "espn":
            out.append("- To adopt this fit, copy it over the packaged valuation/fitted_params.json "
                       "(see `fm backtest fit`).")
        else:
            out.append(f"- The app ships the ESPN-scoring fit for every league; these `{fitted.get('scoring')}` "
                       "differences are scoring-specific and not applied.")
    ins_s = summarize(rows, "inseason", "skaters")
    if ins_s.get("fm_shrink_only") and ins_s.get("fm_current") and \
            ins_s["fm_shrink_only"]["mae"] < ins_s["fm_current"]["mae"]:
        out.append("- The app's current recency blend is worse than no recency blend at all on held-out "
                   f"checkpoints (MAE {ins_s['fm_current']['mae']:.3f} vs {ins_s['fm_shrink_only']['mae']:.3f}).")
    return out


def render(rows: Sequence[ResultRow], info: Mapping[str, Any], fitted: Mapping[str, Any] | None,
           fits: Mapping[int, Any] | None = None) -> str:
    seasons = info.get("seasons") or []
    lines = [f"# Projection backtest ({info.get('scoring')} scoring)", ""]
    lines.append(f"Seasons evaluated: {season_label(seasons[0])} to {season_label(seasons[-1])} "
                 f"({len(seasons)}); history table: {season_label(info['table_seasons'][0])} to "
                 f"{season_label(info['table_seasons'][-1])}." if seasons else "No seasons evaluated.")
    if info.get("unscored_stats"):
        lines.append(f"Not in NHL season data (weight ignored): {', '.join(info['unscored_stats'])}.")
    lines += ["Population: players with >= 20 GP in both N-1 and N (same position group); projections use "
              "seasons < N only. FPG = fantasy points per game played.", ""]
    lines += ["## Verdict", ""] + verdict(rows) + [""]
    lines += ["## Preseason: skaters (mean over seasons)", ""] + summary_table(rows, "preseason", "skaters", PRESEASON_ORDER)
    lines += ["", "### Per season (skaters; bold = lowest MAE)", ""] + per_season_table(rows, "preseason", "skaters", PRESEASON_ORDER)
    lines += ["", "## Preseason: goalies", ""] + summary_table(rows, "preseason", "goalies", PRESEASON_ORDER)
    lines += ["", "### Per season (goalies)", ""] + per_season_table(rows, "preseason", "goalies", PRESEASON_ORDER)
    if any(r.kind == "inseason" for r in rows):
        lines += ["", "## In-season checkpoints (rest-of-season FPG)", "",
                  "Players with >= 1 GP before the checkpoint and >= 10 GP after it. `preseason` = the app's "
                  "baseline alone, `to_date` = season-to-date FPG, `fm_shrink_only` = the app's shrinkage with no "
                  "recency blend, `fm_current` = the app's shrinkage + recency blend (blend.py functions), "
                  "`fm_fitted` = fitted k and recency weights (leave-one-season-out).", ""]
        lines += ["### Skaters", ""] + summary_table(rows, "inseason", "skaters", INSEASON_MODELS)
        lines += ["", "### Goalies", ""] + summary_table(rows, "inseason", "goalies", INSEASON_MODELS)
        lines += ["", "### Per checkpoint (skaters)", ""] + per_season_table(rows, "inseason", "skaters", INSEASON_MODELS)
    if fitted:
        lines += ["", "## Fitted parameters", ""] + fitted_section(fitted)
    recs = recommendations(rows, fitted)
    if recs:
        lines += ["", "## Recommended constant changes", ""] + recs
    lines += ["", "## Caveats", "",
              "- No historical ESPN/Fantrax projections exist, so `fm_current` is the app's no-projection path "
              "(`valuate.baseline_rates`: the last three seasons weighted 5/4/3, shrunk toward the positional mean, "
              "times the age factor). The commercial projections are archived daily by "
              "`fm backtest archive` and can be scored at season end.",
              "- `fm_current` uses the packaged constants (valuation/fitted_params.json), fitted on every season "
              "including the evaluated ones, so it is slightly in-sample; `fm_multi` / `fm_fitted` are re-fitted "
              "before each target season on earlier seasons only.",
              "- Rookies and players with < 20 GP last season are not in the preseason population.",
              "- The age curve uses survivors (>= 20 GP both seasons); decline at 33+ is if anything understated.",
              "- Goalie FPG per game played is noisy (relief appearances, team effects); rank correlations are low for every model."]
    if info.get("data"):
        d = info["data"]
        lines += ["", f"Data build: {d}"]
    return "\n".join(lines) + "\n"


def write_report(path: Path, rows: Sequence[ResultRow], info: Mapping[str, Any],
                 fitted: Mapping[str, Any] | None, fits: Mapping[int, Any] | None = None) -> Path:
    path.write_text(render(rows, info, fitted, fits), encoding="utf-8")
    return path
