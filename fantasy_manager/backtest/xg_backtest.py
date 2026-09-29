"""Does shrinking goals toward expected goals (MoneyPuck ixG) improve next-season projections?

Data: MoneyPuck skater season summaries 2018-19..2025-26 (``providers.moneypuck``, 30-day cache
for finished seasons) joined by NHL player id to the stored NHL history table
(``backtest.data.load_table``; build it with ``fm backtest data``).

1. **Projection test.** For each target season N (2019-20..2025-26) and skater with >= 20 GP in
   N-1 and N (``evaluate.preseason_predictions``), project FPG with

   * ``naive``       - last season's rates, and
   * ``fm_current``  - the app's preseason baseline (``valuation.valuate.baseline_rates``: seasons
     N-1..N-3 weighted 5/4/3, shrunk to the positional mean, aged),

   each once as is and once with every history season's goal rate first shrunk toward that
   season's ixG rate (``valuation.regression.shrink_goals``: G' = (GP*G + k*ixG/GP)/(GP + k)) for
   k in ``K_GRID`` (k = inf replaces goals with ixG). Seasons before 2018-19 have no ixG and are
   left as they are. MAE / Spearman per season and scoring preset, plus a "big gap" pool of
   players whose |goals - ixG| in N-1 was >= ``BIG_GAP`` goals.

2. **Shooting-% regression.** Pairs (N-1, N) with >= ``MIN_SOG`` shots on goal in both: does
   finishing above expected in N-1 ((G - ixG)/SOG) predict the change in shooting % in N, and does
   ixG/SOG (shot quality) predict next season's shooting % better than last season's shooting %?

3. **Luck terms.** How much of ``luck_signals``' goal luck ((G - ixG)/GP) and assist luck (A/GP
   scaled by 5-on-5 on-ice shooting % above the positional norm) disappears the next season
   (skaters with >= 40 GP both seasons)?

Report: ``<fm_data_dir>/backtest/xg-report-YYYY-MM-DD.md``. Run with
``python -m fantasy_manager.backtest.xg_backtest``.
"""
from __future__ import annotations

import argparse
import math
import time
from dataclasses import dataclass, field, replace
from datetime import date
from pathlib import Path
from typing import Any, Callable, Iterable, Sequence

from ..providers.moneypuck import CREDIT, MoneyPuckClient, MpSkater, onice_sh_norms, xg_calibration
from ..scoring import PointsScoring
from ..valuation.regression import ASSIST_LUCK_CLAMP, K_GOALS, shrink_goals
from .data import PlayerSeason, SeasonTable, backtest_dir, load_table, season_label
from .evaluate import MIN_GP, metrics, pearson, preseason_predictions, spearman
from .models import FmCurrent, NaiveLastSeason, ProjectionContext
from .scoring import PRESETS, scorer

FIRST_MP_YEAR = 2018
LAST_MP_YEAR = 2025
K_GRID: tuple[float, ...] = (5.0, 10.0, 15.0, 30.0, 60.0, 120.0, math.inf)
BIG_GAP = 5.0            # |goals - ixG| in N-1 for the "big gap" pool
MIN_SOG = 50
MIN_GP_ASSISTS = 40
SCORINGS = ("espn", "fantrax")

INF = math.inf


def sid(year: int) -> int:
    """2025 -> 20252026."""
    return year * 10000 + year + 1


def k_label(k: float) -> str:
    return "ixG" if k == INF else f"{k:g}"


def arm_name(base: str, k: float, calibrate: bool = False) -> str:
    """'fm_current+xg15', 'naive+xgcal60', 'fm_current+xgixG'."""
    return f"{base}+xg{'cal' if calibrate else ''}{k_label(k)}"


# --------------------------------------------------------------------------- data

@dataclass
class XgSeason:
    all: dict[int, MpSkater]        # situation "all" by NHL id
    five: dict[int, MpSkater]       # situation "5on5"
    cal: dict[str, float] = field(default_factory=dict)   # group -> league goals / ixG


def load_xg(client: MoneyPuckClient, years: Iterable[int],
            log: Callable[[str], None] | None = None) -> dict[int, XgSeason]:
    """NHL season id -> MoneyPuck skater rows (all situations / 5-on-5)."""
    out: dict[int, XgSeason] = {}
    for y in years:
        t0 = time.perf_counter()
        rows = client.skaters(y, situation=None)
        alls = [r for r in rows if r.situation == "all"]
        out[sid(y)] = XgSeason({r.nhl_id: r for r in alls},
                               {r.nhl_id: r for r in rows if r.situation == "5on5"}, xg_calibration(alls))
        if log:
            log(f"MoneyPuck {season_label(sid(y))}: {len(out[sid(y)].all)} skaters "
                f"({time.perf_counter() - t0:.1f}s)")
    return out


def adjust_row(row: PlayerSeason, xg: dict[int, XgSeason], k: float, calibrate: bool = False) -> PlayerSeason:
    """``row`` with its goal total shrunk toward ixG (unchanged for goalies / no ixG).
    ``calibrate`` scales ixG by the season's league goals / ixG ratio of the player's group."""
    if row.is_goalie or row.gp <= 0 or "G" not in row.stats:
        return row
    season = xg.get(row.season)
    mp = season.all.get(row.player_id) if season else None
    if mp is None or mp.gp <= 0:
        return row
    scale = season.cal.get(mp.group, 1.0) if calibrate else 1.0
    rates = shrink_goals(row.per_game(), mp.ixg / mp.gp * scale, row.gp, k)
    return replace(row, stats={key: v * row.gp for key, v in rates.items()})


class XgShrunk:
    """Wraps a backtest model: the history's goal rates are shrunk toward ixG first."""

    def __init__(self, base: Any, xg: dict[int, XgSeason], k: float = K_GOALS, calibrate: bool = False):
        self.base = base
        self.xg = xg
        self.k = k
        self.calibrate = calibrate
        self.name = arm_name(base.name, k, calibrate)

    def rates(self, history: list[PlayerSeason], age: float | None, ctx: ProjectionContext):
        return self.base.rates([adjust_row(h, self.xg, self.k, self.calibrate) for h in history], age, ctx)


# --------------------------------------------------------------------------- 1. projections

@dataclass
class ProjRow:
    scoring: str
    season: int
    pool: str               # skaters / big_gap
    model: str
    n: int
    mae: float
    spearman: float
    bias: float


def evaluate_projections(table: SeasonTable, xg: dict[int, XgSeason], seasons: Sequence[int],
                         scoring_name: str, scoring: PointsScoring, k_grid: Sequence[float] = K_GRID
                         ) -> list[ProjRow]:
    bases = [NaiveLastSeason(), FmCurrent()]
    models: list[Any] = list(bases)
    for b in bases:
        for cal in (False, True):
            models.extend(XgShrunk(b, xg, k, cal) for k in k_grid)
    out: list[ProjRow] = []
    for season in seasons:
        preds = [p for p in preseason_predictions(table, scoring, season, models) if p.group != "G"]
        prev = xg.get(season - 10001)
        big = {p.player_id for p in preds
               if prev is not None and p.player_id in prev.all
               and abs(prev.all[p.player_id].goals_minus_ixg) >= BIG_GAP}
        for pool, keep in (("skaters", None), ("big_gap", big)):
            by_model: dict[str, list] = {}
            for p in preds:
                if keep is None or p.player_id in keep:
                    by_model.setdefault(p.model, []).append(p)
            for m in models:
                ps = by_model.get(m.name, [])
                if not ps:
                    continue
                mt = metrics([p.projected for p in ps], [p.actual for p in ps], top=False)
                out.append(ProjRow(scoring_name, season, pool, m.name, mt.n, mt.mae, mt.spearman, mt.bias))
    return out


# --------------------------------------------------------------------------- 2./3. regression

@dataclass
class ShPair:
    season: int             # N
    gmx_per_sog: float      # (G - ixG) / SOG in N-1
    gmx_per_gp_prev: float
    gmx_per_gp_next: float
    sh_prev: float
    sh_next: float
    xsh_prev: float         # ixG / SOG in N-1


def shooting_pairs(xg: dict[int, XgSeason], min_sog: int = MIN_SOG) -> list[ShPair]:
    out: list[ShPair] = []
    for s in sorted(xg):
        prev = xg.get(s - 10001)
        if prev is None:
            continue
        for pid, b in xg[s].all.items():
            a = prev.all.get(pid)
            if a is None or a.sog < min_sog or b.sog < min_sog or a.gp <= 0 or b.gp <= 0:
                continue
            out.append(ShPair(s, (a.goals - a.ixg) / a.sog, (a.goals - a.ixg) / a.gp, (b.goals - b.ixg) / b.gp,
                              a.goals / a.sog, b.goals / b.sog, a.ixg / a.sog))
    return out


def ols_slope(x: Sequence[float], y: Sequence[float]) -> float:
    n = len(x)
    if n < 2:
        return float("nan")
    mx, my = sum(x) / n, sum(y) / n
    sxx = sum((a - mx) ** 2 for a in x)
    return sum((a - mx) * (b - my) for a, b in zip(x, y)) / sxx if sxx > 0 else float("nan")


def _mae(pred: Sequence[float], actual: Sequence[float]) -> float:
    return sum(abs(p - a) for p, a in zip(pred, actual)) / len(pred) if pred else float("nan")


def shooting_summary(pairs: Sequence[ShPair]) -> dict[str, float]:
    if not pairs:
        return {"n": 0}
    x = [p.gmx_per_sog for p in pairs]
    dy = [p.sh_next - p.sh_prev for p in pairs]
    league = sum(p.sh_prev for p in pairs) / len(pairs)
    sh_dev = [p.sh_prev - league for p in pairs]
    nxt = [p.sh_next for p in pairs]
    # best 50/50-style blend of last season's sh% and xsh% for predicting next season's sh%
    blend = {w: _mae([w * p.sh_prev + (1 - w) * p.xsh_prev for p in pairs], nxt) for w in
             (0.0, 0.25, 0.5, 0.75, 1.0)}
    best_w = min(blend, key=blend.get)
    return {
        "n": len(pairs),
        "corr_gmx_vs_dsh": pearson(x, dy),
        "spearman_gmx_vs_dsh": spearman(x, dy),
        "slope_gmx_vs_dsh": ols_slope(x, dy),
        "corr_shdev_vs_dsh": pearson(sh_dev, dy),
        "corr_sh_prev_next": pearson([p.sh_prev for p in pairs], nxt),
        "corr_xsh_prev_next": pearson([p.xsh_prev for p in pairs], nxt),
        "mae_sh_prev": blend[1.0],
        "mae_xsh_prev": blend[0.0],
        "best_blend_w_sh": best_w,
        "mae_best_blend": blend[best_w],
        "corr_gmx_gp_persistence": pearson([p.gmx_per_gp_prev for p in pairs], [p.gmx_per_gp_next for p in pairs]),
    }


def luck_summary(table: SeasonTable, xg: dict[int, XgSeason], min_gp: int = MIN_GP_ASSISTS
                 ) -> dict[str, float]:
    """How much of ``valuation.regression``'s luck terms goes away the next season (skaters with
    >= ``min_gp`` GP in N-1 and N):

    * goals: slope of the change in G/GP (N-1 -> N) on (G - ixG)/GP in N-1 (-1 = all of it)
    * assists: slope of the change in A/GP on the assist-luck term A/GP * clamp(1 - norm /
      on-ice sh%) of N-1 (5v5 on-ice sh%, norm = that season's positional 5v5 norm), plus the
      plain correlation of on-ice sh% minus the norm with the A/GP change."""
    gx: list[float] = []
    gy: list[float] = []
    ax: list[float] = []
    ay: list[float] = []
    dx: list[float] = []
    lo, hi = ASSIST_LUCK_CLAMP
    for s in sorted(xg):
        prev = xg.get(s - 10001)
        if prev is None:
            continue
        norms = onice_sh_norms(prev.five.values())
        for pid, mp in prev.all.items():
            a = table.get(pid, s - 10001)
            b = table.get(pid, s)
            if a is None or b is None or a.is_goalie or a.gp < min_gp or b.gp < min_gp or mp.gp <= 0:
                continue
            gx.append((mp.goals - mp.ixg) / mp.gp)
            gy.append(b.stats.get("G", 0.0) / b.gp - a.stats.get("G", 0.0) / a.gp)
            five = prev.five.get(pid)
            if five is None or not five.onice_sh_pct or five.group not in norms:
                continue
            a_pg = a.stats.get("A", 0.0) / a.gp
            ax.append(a_pg * min(hi, max(lo, 1.0 - norms[five.group] / five.onice_sh_pct)))
            ay.append(b.stats.get("A", 0.0) / b.gp - a_pg)
            dx.append(five.onice_sh_pct - norms[five.group])
    nan = float("nan")
    return {"n_goals": len(gx), "corr_goal_luck": pearson(gx, gy) if gx else nan,
            "slope_goal_luck": ols_slope(gx, gy) if gx else nan,
            "n": len(ax), "corr_assist_luck": pearson(ax, ay) if ax else nan,
            "slope_assist_luck": ols_slope(ax, ay) if ax else nan,
            "corr_onice_delta_vs_dA": pearson(dx, ay) if dx else nan}


def season_norms(xg: dict[int, XgSeason]) -> dict[int, dict[str, float]]:
    """Season id -> 5v5 on-ice shooting % norms (F / D)."""
    return {s: onice_sh_norms(x.five.values()) for s, x in sorted(xg.items())}


# --------------------------------------------------------------------------- report

def _avg(rows: Sequence[ProjRow], attr: str) -> float:
    vals = [getattr(r, attr) for r in rows if not math.isnan(getattr(r, attr))]
    return sum(vals) / len(vals) if vals else float("nan")


def summarize(rows: Sequence[ProjRow], scoring: str, pool: str) -> dict[str, dict[str, float]]:
    out: dict[str, dict[str, float]] = {}
    models = []
    for r in rows:
        if r.scoring == scoring and r.pool == pool and r.model not in models:
            models.append(r.model)
    for m in models:
        rs = [r for r in rows if r.scoring == scoring and r.pool == pool and r.model == m]
        out[m] = {"mae": _avg(rs, "mae"), "spearman": _avg(rs, "spearman"), "bias": _avg(rs, "bias"),
                  "n": sum(r.n for r in rs), "seasons": len(rs)}
    return out


HEADLINE_ARMS: tuple[tuple[str, float, bool], ...] = (
    ("fm_current", K_GOALS, False), ("fm_current", K_GOALS, True), ("fm_current", 5.0, True),
    ("naive", K_GOALS, False), ("naive", 60.0, True))


def headline(rows: Sequence[ProjRow], scoring: str, base: str = "fm_current", k: float = K_GOALS,
             pool: str = "skaters", calibrate: bool = False) -> dict[str, float]:
    """Average MAE / Spearman change of ``base``+xg(k) vs ``base`` and seasons improved."""
    name = arm_name(base, k, calibrate)
    pairs = []
    for r in rows:
        if r.scoring == scoring and r.pool == pool and r.model == base:
            o = next((x for x in rows if x.scoring == scoring and x.pool == pool and x.model == name
                      and x.season == r.season), None)
            if o is not None:
                pairs.append((r, o))
    if not pairs:
        return {}
    d_mae = [o.mae - r.mae for r, o in pairs]
    d_sp = [o.spearman - r.spearman for r, o in pairs]
    return {"seasons": len(pairs), "mae_base": sum(r.mae for r, _ in pairs) / len(pairs),
            "mae_xg": sum(o.mae for _, o in pairs) / len(pairs), "d_mae": sum(d_mae) / len(pairs),
            "d_mae_pct": 100 * sum(d_mae) / sum(r.mae for r, _ in pairs),
            "d_spearman": sum(d_sp) / len(pairs), "mae_wins": sum(1 for d in d_mae if d < 0),
            "spearman_wins": sum(1 for d in d_sp if d > 0)}


def _f(v: Any, fmt: str = ".4f") -> str:
    if v is None or (isinstance(v, float) and math.isnan(v)):
        return "-"
    return format(v, fmt)


def render_report(rows: Sequence[ProjRow], sh: dict[str, float], ast: dict[str, float],
                  norms: dict[int, dict[str, float]], seasons: Sequence[int], years: Sequence[int],
                  counts: dict[int, int], runtime: float, today: date) -> str:
    L: list[str] = [f"# Expected-goals backtest ({today.isoformat()})", "",
                    f"{CREDIT} (season summaries {years[0]}-{str(years[0] + 1)[-2:]}.."
                    f"{years[-1]}-{str(years[-1] + 1)[-2:]}, {sum(counts.values())} skater rows). "
                    f"Target seasons {season_label(seasons[0])}..{season_label(seasons[-1])}; "
                    f"skaters with >= {MIN_GP} GP in N-1 and N. Runtime {runtime:.0f}s.", "",
                    "Goal rate of every history season shrunk toward that season's ixG rate: "
                    "G' = (GP*G + k*ixG/GP)/(GP + k); `xgixG` replaces goals with ixG. Seasons "
                    "before 2018-19 carry no ixG and stay unadjusted.", ""]
    L.append("## Headline (each arm vs its base model; `cal` = ixG scaled by the season's league "
             "goals / ixG of the player's group)")
    L.append("")
    L.append("| scoring | pool | arm | seasons | MAE base | MAE +xG | dMAE | dMAE % | dSpearman | MAE wins "
             "| Spearman wins |")
    L.append("|---|---|---|---|---|---|---|---|---|---|---|")
    for sc in SCORINGS:
        for pool in ("skaters", "big_gap"):
            for base, k, cal in HEADLINE_ARMS:
                h = headline(rows, sc, base, k, pool, cal)
                if not h:
                    continue
                L.append(f"| {sc} | {pool} | {arm_name(base, k, cal)} | {h['seasons']} | {h['mae_base']:.4f} | "
                         f"{h['mae_xg']:.4f} | {h['d_mae']:+.4f} | {h['d_mae_pct']:+.2f}% | "
                         f"{h['d_spearman']:+.4f} | {h['mae_wins']}/{h['seasons']} | "
                         f"{h['spearman_wins']}/{h['seasons']} |")
    L.append("")
    for sc in SCORINGS:
        for pool in ("skaters", "big_gap"):
            summ = summarize(rows, sc, pool)
            if not summ:
                continue
            L.append(f"## Average over seasons: {sc}, {pool}")
            L.append("")
            L.append("| model | MAE | Spearman | bias | n |")
            L.append("|---|---|---|---|---|")
            for m, v in summ.items():
                L.append(f"| {m} | {v['mae']:.4f} | {v['spearman']:.4f} | {v['bias']:+.4f} | {v['n']} |")
            L.append("")
    for sc in SCORINGS:
        L.append(f"## Per season: {sc}, skaters (fm_current vs +xG k=15 / ixG)")
        L.append("")
        L.append("| season | n | MAE base | MAE k=15 | MAE ixG | Spearman base | Spearman k=15 | Spearman ixG |")
        L.append("|---|---|---|---|---|---|---|---|")
        for s in seasons:
            get = {r.model: r for r in rows if r.scoring == sc and r.pool == "skaters" and r.season == s}
            b, x, f = get.get("fm_current"), get.get("fm_current+xg15"), get.get("fm_current+xgixG")
            if b is None:
                continue
            L.append(f"| {season_label(s)} | {b.n} | {b.mae:.4f} | {_f(x and x.mae)} | {_f(f and f.mae)} | "
                     f"{b.spearman:.4f} | {_f(x and x.spearman)} | {_f(f and f.spearman)} |")
        L.append("")
    L.append(f"## Shooting-% regression (pairs with >= {MIN_SOG} SOG both seasons, n = {sh.get('n', 0)})")
    L.append("")
    if sh.get("n"):
        L += [f"- corr((G - ixG)/SOG in N-1, sh% change N-1 -> N): **{sh['corr_gmx_vs_dsh']:.3f}** "
              f"(Spearman {sh['spearman_gmx_vs_dsh']:.3f}); slope {sh['slope_gmx_vs_dsh']:.2f} "
              "(-1 = the whole excess disappears)",
              f"- for comparison corr(sh% minus league mean in N-1, sh% change): {sh['corr_shdev_vs_dsh']:.3f}",
              f"- predicting next season's sh%: corr with last sh% {sh['corr_sh_prev_next']:.3f} vs with ixG/SOG "
              f"{sh['corr_xsh_prev_next']:.3f}; MAE last sh% {sh['mae_sh_prev']:.4f}, ixG/SOG "
              f"{sh['mae_xsh_prev']:.4f}, best blend (weight {sh['best_blend_w_sh']:.2f} on last sh%) "
              f"{sh['mae_best_blend']:.4f}",
              f"- persistence of finishing: corr((G - ixG)/GP N-1, same in N) = {sh['corr_gmx_gp_persistence']:.3f}",
              ""]
    L.append(f"## Luck terms of `valuation.regression` (>= {MIN_GP_ASSISTS} GP both seasons)")
    L.append("")
    L += [f"- goal luck (G - ixG)/GP in N-1 vs change in G/GP (n = {ast.get('n_goals', 0)}): corr "
          f"**{_f(ast.get('corr_goal_luck'), '.3f')}**, slope {_f(ast.get('slope_goal_luck'), '.2f')} "
          "(-1 = all of the goal luck disappears the next season)",
          f"- assist luck A/GP * clamp(1 - norm / on-ice sh%) vs change in A/GP (n = {ast.get('n', 0)}): corr "
          f"**{_f(ast.get('corr_assist_luck'), '.3f')}**, slope {_f(ast.get('slope_assist_luck'), '.2f')}",
          f"- corr(5v5 on-ice sh% minus positional norm in N-1, A/GP change): "
          f"{_f(ast.get('corr_onice_delta_vs_dA'), '.3f')}",
          "- 5v5 on-ice sh% norms (F / D) by season: " + ", ".join(
              f"{season_label(s)} {n.get('F', float('nan')):.4f} / {n.get('D', float('nan')):.4f}"
              for s, n in norms.items()), ""]
    return "\n".join(L) + "\n"


def run_xg_backtest(cache: Any, data_dir: Path | str, years: Sequence[int] | None = None,
                    seasons: Sequence[int] | None = None, client: MoneyPuckClient | None = None,
                    table: SeasonTable | None = None, today: date | None = None,
                    log: Callable[[str], None] | None = None, write: bool = True) -> tuple[Path | None, dict[str, Any]]:
    """Run all three tests; write the markdown report (``write``) and return (path, results)."""
    t0 = time.perf_counter()
    today = today or date.today()
    years = list(years or range(FIRST_MP_YEAR, LAST_MP_YEAR + 1))
    client = client or MoneyPuckClient.from_cache(cache, today)
    table = table if table is not None else load_table(data_dir)
    if not table.seasons:
        raise RuntimeError("NHL history table is empty; run `fm backtest data` first")
    xg = load_xg(client, years, log)
    seasons = list(seasons or [sid(y) + 10001 for y in years if sid(y) + 10001 in table.by_season])
    rows: list[ProjRow] = []
    for sc in SCORINGS:
        rows += evaluate_projections(table, xg, seasons, sc, scorer(PRESETS[sc]))
        if log:
            h = headline(rows, sc)
            if h:
                log(f"{sc}: fm_current MAE {h['mae_base']:.4f} -> {h['mae_xg']:.4f} with xG k=15 "
                    f"({h['d_mae_pct']:+.2f}%), Spearman {h['d_spearman']:+.4f}")
    sh = shooting_summary(shooting_pairs(xg))
    ast = luck_summary(table, xg)
    norms = season_norms(xg)
    runtime = time.perf_counter() - t0
    results = {"rows": rows, "shooting": sh, "assists": ast, "norms": norms, "seasons": seasons,
               "headline": {sc: headline(rows, sc) for sc in SCORINGS}, "runtime": runtime}
    path = None
    if write:
        path = backtest_dir(data_dir) / f"xg-report-{today.isoformat()}.md"
        path.write_text(render_report(rows, sh, ast, norms, seasons, years,
                                      {y: len(xg[sid(y)].all) for y in years}, runtime, today), encoding="utf-8")
    return path, results


def main(argv: Sequence[str] | None = None) -> None:
    ap = argparse.ArgumentParser(description="MoneyPuck expected-goals backtest")
    ap.add_argument("--data-dir", default=None, help="default: FM_DATA_DIR from settings")
    ap.add_argument("--from-year", type=int, default=FIRST_MP_YEAR)
    ap.add_argument("--to-year", type=int, default=LAST_MP_YEAR)
    args = ap.parse_args(argv)
    from ..cache import HttpCache
    if args.data_dir:
        data_dir, offline = Path(args.data_dir), False
    else:
        from ..config import get_settings
        s = get_settings()
        data_dir, offline = Path(s.fm_data_dir), bool(s.fm_offline)
    cache = HttpCache(data_dir, offline=offline)
    try:
        path, _ = run_xg_backtest(cache, data_dir, range(args.from_year, args.to_year + 1), log=print)
    finally:
        cache.close()
    print(f"report: {path}")


if __name__ == "__main__":
    main()
