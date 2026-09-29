"""Fill every skater's deployment fields (Player.toi_per_game, pp_toi_per_game, pp_share,
toi_trend, pp_share_trend) from the harness ledger's per-game rows.

For each skater with an NHL id (goalies are skipped):

* ``toi_per_game`` / ``pp_toi_per_game`` (minutes) and ``pp_share`` (share of the team's PP
  time, 0..1) are this season's per-game averages: from the ledger, or from the NHL season
  report (``isGame=false``) when that covers more games (days the ledger missed);
* ``toi_trend`` / ``pp_share_trend`` = the last ``last_n`` GP minus the baseline (the season's
  earlier games, else last season's averages): ``harness.deployment.deployment_summary``.
  They need ledger rows (at least ``last_n`` games this season), otherwise they stay None.
* No games this season (preseason, or a player yet to dress): last season's averages from the
  NHL season report, trends None.

Network access is limited to the season reports (``nhl``, optional; 1-4 cached requests). Every
failure becomes a warning on ``ctx.warnings``. ``role_change_alerts`` is
``recommend.flags.recommend_role_alerts``.
"""
from __future__ import annotations

from datetime import date
from typing import Any, Mapping

from ..harness.deployment import DEFAULT_LAST_N, MIN_BASE_GP, deployment_summaries
from ..harness.ledger import Ledger
from ..models import LeagueContext, Player, Recommendation
from .nhl import NhlSkaterDeploymentSeason, current_season, prior_season


def _minutes(seconds: float | None) -> float | None:
    return None if seconds is None else round(float(seconds) / 60.0, 3)


def _season_report(nhl: Any, season: int, ctx: LeagueContext, label: str) -> dict[int, NhlSkaterDeploymentSeason]:
    try:
        return nhl.skater_deployment_season(season)
    except Exception as e:  # noqa: BLE001 - best effort, like every enrichment step
        ctx.warnings.append(f"Deployment ({label} season report) unavailable: {type(e).__name__}: {e}")
        return {}


def _fill_from_report(p: Player, rep: NhlSkaterDeploymentSeason) -> None:
    p.toi_per_game = _minutes(rep.toi_per_game)
    p.pp_toi_per_game = _minutes(rep.pp_toi_per_game)
    p.pp_share = None if rep.pp_share is None else round(float(rep.pp_share), 4)
    p.toi_trend = None
    p.pp_share_trend = None


def enrich_deployment(ctx: LeagueContext, ledger: Ledger | None, as_of: date | None = None, nhl: Any = None,
                      last_n: int = DEFAULT_LAST_N, fallback: bool = True) -> dict[str, Any]:
    """Set the deployment fields of every skater with an NHL id (in place; see module docstring).

    Returns {"skaters", "ledger", "season_report", "prior_season", "missing", "trends",
    "details": {cid: deployment summary}}; pass ``details`` to ``role_change_alerts`` for exact
    recent / baseline numbers in the alert reasons."""
    as_of = as_of or ctx.as_of
    skaters = [p for p in ctx.all_players() if p.nhl_id is not None and not p.is_goalie]
    ids = {int(p.nhl_id) for p in skaters}
    season = current_season(as_of)
    cur: dict[int, NhlSkaterDeploymentSeason] = {}
    prior: dict[int, NhlSkaterDeploymentSeason] = {}
    if nhl is not None and fallback and ids:
        cur = _season_report(nhl, season, ctx, "current")
    rows_gp: dict[int, int] = {}
    if ledger is not None and ids:
        rows_gp = {pid: s["gp"] for pid, s in deployment_summaries(ledger, ids, as_of, last_n).items()}
    # last season's averages: the trend baseline early in the season and the preseason fallback
    need_prior = any(rows_gp.get(pid, 0) < last_n + MIN_BASE_GP for pid in ids)
    if nhl is not None and fallback and need_prior:
        prior = _season_report(nhl, prior_season(season), ctx, "prior")
    priors = {pid: {"toi": _minutes(r.toi_per_game), "pp_share": r.pp_share}
              for pid, r in prior.items() if r.gp > 0}
    summaries = deployment_summaries(ledger, ids, as_of, last_n, priors) if ledger is not None and ids else {}

    counts = {"skaters": len(skaters), "ledger": 0, "season_report": 0, "prior_season": 0, "missing": 0, "trends": 0}
    details: dict[str, dict[str, Any]] = {}
    for p in skaters:
        pid = int(p.nhl_id)
        s = summaries.get(pid)
        rep = cur.get(pid)
        if s and s["gp"] > 0:
            p.toi_per_game, p.pp_toi_per_game, p.pp_share = s["season_toi"], s["season_pp_toi"], s["season_pp_share"]
            if rep is not None and rep.gp > s["gp"]:        # the ledger missed some days
                _fill_from_report(p, rep)
                counts["season_report"] += 1
            else:
                counts["ledger"] += 1
            p.toi_trend, p.pp_share_trend = s["toi_trend"], s["pp_share_trend"]
            details[p.cid] = s
            if s["toi_trend"] is not None or s["pp_share_trend"] is not None:
                counts["trends"] += 1
        elif rep is not None and rep.gp > 0:
            _fill_from_report(p, rep)
            counts["season_report"] += 1
        elif pid in prior and prior[pid].gp > 0:
            _fill_from_report(p, prior[pid])
            counts["prior_season"] += 1
        else:
            counts["missing"] += 1
    return {**counts, "details": details}


def role_change_alerts(ctx: LeagueContext, details: Mapping[str, Mapping[str, Any]] | None = None
                       ) -> list[Recommendation]:
    """Role-change alerts (kind "alert"): see ``recommend.flags.recommend_role_alerts``."""
    from ..recommend.flags import recommend_role_alerts
    return recommend_role_alerts(ctx, details=details)
