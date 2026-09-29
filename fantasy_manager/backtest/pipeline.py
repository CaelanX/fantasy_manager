"""Orchestration used by ``fm backtest``: build data, evaluate, fit, write report / params."""
from __future__ import annotations

import json
import time
from datetime import date, datetime
from pathlib import Path
from typing import Any, Callable, Sequence

from ..valuation.blend import K_BASELINE, K_INSEASON, RECENCY_WEIGHTS
from ..valuation.params import PARAMS_FILE
from .data import (LAST_SEASON, CountingFetcher, SeasonTable, backtest_dir, build_table, build_windows,
                   load_table, load_windows, prev_season, season_range)
from .evaluate import evaluate_inseason, evaluate_preseason, inseason_observations
from .fit import GROUPS, REPORT_AGES, dynasty_anchors, fit_inseason, fit_preseason
from .scoring import load_scoring, missing_weights, scorer

Log = Callable[[str], None]

EVAL_FIRST = 20162017
INSEASON_FIRST = 20212022


def default_eval_seasons(table: SeasonTable) -> list[int]:
    return [s for s in table.seasons if s >= EVAL_FIRST and prev_season(s) in table.by_season]


def default_inseason_seasons(last: int = LAST_SEASON) -> list[int]:
    return season_range(INSEASON_FIRST, last)


def refresh_data(cache: Any, data_dir: Path, seasons: Sequence[int], inseason_seasons: Sequence[int],
                 force: bool = False, log: Log = print) -> dict[str, Any]:
    from ..providers.nhl import NhlClient

    fetcher = CountingFetcher(cache)
    client = NhlClient(fetch_json=fetcher)
    t0 = time.perf_counter()
    table = build_table(client, data_dir, seasons, force=force, log=log)
    windows = build_windows(client, data_dir, inseason_seasons, force=force, log=log) if inseason_seasons else {}
    return {"seasons": table.seasons, "rows": len(table.rows),
            "checkpoints": sum(len(v) for v in windows.values()),
            "network_requests": fetcher.network, "cache_hits": fetcher.cached,
            "network_seconds": round(fetcher.seconds, 1), "wall_seconds": round(time.perf_counter() - t0, 1)}


def _suffix(scoring_name: str) -> str:
    """'' for the default ESPN preset (the files the app reads), '-<name>' otherwise."""
    return "" if scoring_name == "espn" else f"-{scoring_name}"


def fitted_params_path(data_dir: Path, scoring_name: str = "espn") -> Path:
    return backtest_dir(data_dir) / f"fitted_params{_suffix(scoring_name)}.json"


def run_fit(data_dir: Path, scoring_name: str = "espn", boot: int = 200,
            inseason_seasons: Sequence[int] | None = None, log: Log = print) -> dict[str, Any]:
    """Fit every constant on all stored history and write ``fitted_params.json``."""
    name, cfg = load_scoring(scoring_name)
    sc = scorer(cfg)
    table = load_table(data_dir)
    if not table.seasons:
        raise RuntimeError("no backtest data; run `fm backtest data` first")
    targets = [s for s in table.seasons if prev_season(s) in table.by_season]
    params, info = fit_preseason(table, sc, targets, boot=boot)
    windows = load_windows(data_dir)
    ins_seasons = [s for s in (inseason_seasons or sorted(windows)) if s in windows]
    ins = fit_inseason(inseason_observations(table, windows, sc, ins_seasons)) if ins_seasons else None
    curves = info["curves"]
    out: dict[str, Any] = {
        "generated": datetime.now().isoformat(timespec="seconds"),
        "scoring": name,
        "unscored_stats": missing_weights(cfg),
        "train_target_seasons": targets,
        "preseason": {
            "pairs": info["pairs"],
            "k": params.k,
            "k_skater_pooled": info["k_skater_pooled"],
            "k_multi": params.k_multi,
            "mae_at_fitted_k_multi": {g: round(info["k_multi_curves"][g].get(params.k_multi[g], float("nan")), 5)
                                      for g in GROUPS},
            "mae_at_fitted_k": {g: round(info["k_curves"][g].get(params.k[g], float("nan")), 5) for g in GROUPS},
            # the app's baseline is the 3-season one, so its k is compared on the k_multi curve
            "mae_at_app_k": {g: round(info["k_multi_curves"][g].get(float(K_BASELINE[g]), float("nan")), 5)
                             for g in GROUPS},
            "app_k": {g: float(K_BASELINE[g]) for g in GROUPS},
            "mae_by_k": {g: {str(int(k)): round(v, 5) for k, v in curve.items() if k in (0, 6, 12, 20, 30, 40, 50, 60, 80, 100, 150, 200, 300)}
                         for g, curve in info["k_curves"].items()},
        },
        "inseason": None if ins is None else {
            "seasons": ins_seasons,
            "k_skater": ins["k_skater"], "k_goalie": ins["k_goalie"],
            "recency_weights": ins["recency_weights"],
            "app": {"k_skater": K_INSEASON["skater"], "k_goalie": K_INSEASON["goalie"],
                    "recency_weights": dict(RECENCY_WEIGHTS)},
            "mae_app": ins["mae_app"], "mae_fitted": ins["mae_fitted"],
            "mae_shrink_only_fitted_k": ins["mae_shrink_only_fitted_k"],
            "n_skater_obs": ins["n_skater_obs"], "n_goalie_obs": ins["n_goalie_obs"],
        },
        "age_curve": {g: curves[g].to_dict() for g in GROUPS},
        "age_curve_report": {g: {str(a): {"level": round(curves[g].level[a], 3),
                                          "lo": round(curves[g].lo.get(a, float("nan")), 3),
                                          "hi": round(curves[g].hi.get(a, float("nan")), 3),
                                          "n": curves[g].n.get(a, 0)} for a in REPORT_AGES}
                             for g in GROUPS},
        "dynasty_age_curves": {g: dynasty_anchors(curves[g]) for g in GROUPS},
        "fm_fitted": params.to_dict(),
        "notes": [
            "age_curve levels are FPG trajectories normalized to peak = 1.0 (delta method, median "
            "next/this FPG ratio pooled over age +-1, players with >= 20 GP in both seasons).",
            "yoy = expected FPG multiplier from the season at age a (Oct 1) to the next season.",
            "lo/hi = 90% player-clustered bootstrap interval of the level.",
            "dynasty_age_curves has the ((age, multiplier), ...) shape of valuation.dynasty.AGE_CURVES; "
            "for a trajectory use level(age + y) / level(age).",
        ],
    }
    p = fitted_params_path(data_dir, name)
    p.write_text(json.dumps(out, indent=2), encoding="utf-8")
    out["path"] = str(p)
    out["packaged_path"] = str(PARAMS_FILE)
    return out


def load_fitted(data_dir: Path, scoring_name: str = "espn") -> dict[str, Any] | None:
    p = fitted_params_path(data_dir, scoring_name)
    if not p.exists():
        return None
    try:
        return json.loads(p.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def run_evaluation(data_dir: Path, scoring_name: str = "espn", models: Sequence[str] | None = None,
                   seasons: Sequence[int] | None = None, inseason: bool = True,
                   inseason_seasons: Sequence[int] | None = None, today: date | None = None,
                   meta: dict[str, Any] | None = None, log: Log = print) -> dict[str, Any]:
    """Evaluate, write ``results-<date>.json`` and ``report-<date>.md``; returns paths + rows."""
    from .report import write_report

    name, cfg = load_scoring(scoring_name)
    sc = scorer(cfg)
    table = load_table(data_dir)
    if not table.seasons:
        raise RuntimeError("no backtest data; run `fm backtest data` first")
    seasons = [s for s in (seasons or default_eval_seasons(table)) if s in table.by_season]
    t0 = time.perf_counter()
    rows, _ = evaluate_preseason(table, sc, seasons, models, log=log)
    fits: dict[int, Any] = {}
    if inseason:
        windows = load_windows(data_dir)
        ins_seasons = [s for s in (inseason_seasons or sorted(windows)) if s in windows]
        if ins_seasons:
            obs = inseason_observations(table, windows, sc, ins_seasons)
            irows, fits = evaluate_inseason(obs)
            rows += irows
    today = today or date.today()
    out_dir = backtest_dir(data_dir)
    res_path = out_dir / f"results-{today.isoformat()}{_suffix(name)}.json"
    info = {"scoring": name, "weights": cfg.model_dump(), "unscored_stats": missing_weights(cfg),
            "seasons": seasons, "models": list(models or []), "elapsed_seconds": round(time.perf_counter() - t0, 1),
            "table_seasons": table.seasons, **(meta or {})}
    res_path.write_text(json.dumps({"info": info, "rows": [r.to_dict() for r in rows],
                                    "inseason_fits": {str(k): v for k, v in fits.items()}},
                                   indent=1, default=str), encoding="utf-8")
    rep_path = write_report(out_dir / f"report-{today.isoformat()}{_suffix(name)}.md", rows, info,
                            load_fitted(data_dir, name), fits)
    return {"rows": rows, "results": str(res_path), "report": str(rep_path), "info": info}


def latest_report(data_dir: Path) -> Path | None:
    reports = sorted(backtest_dir(data_dir).glob("report-*.md"), key=lambda p: p.stat().st_mtime)
    return reports[-1] if reports else None
