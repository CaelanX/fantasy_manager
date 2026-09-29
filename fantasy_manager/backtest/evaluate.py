"""Score projection models against what actually happened.

Preseason: for each target season N, every player with >= ``min_gp_prev`` GP in N-1 and
>= ``min_gp`` GP in N is projected from seasons < N only and compared with his actual FPG in N.
``fm_fitted`` is re-fitted for every N on pairs ending at N-1 (rolling origin, no look-ahead).

In-season: at Nov 1 / Dec 1 / Jan 1, project rest-of-season FPG from the preseason baseline
(the app's ``fm_current`` baseline: seasons N-1..N-3, fitted k and age factor) + season-to-date + last 30/15/7-day windows, compared with
actual FPG from the checkpoint to season end (players with >= 1 GP to date and
>= ``min_gp_rest`` after). ``fm_fitted`` uses leave-one-season-out fitted k / weights.

Metrics per (season, model, pool): MAE, RMSE, bias (mean projected - actual), Spearman rank
correlation, and top-50 / top-100 precision (share of the projected top N that finished top N;
skaters only).
"""
from __future__ import annotations

import math
from dataclasses import asdict, dataclass
from typing import Any, Callable, Iterable, Sequence

from ..scoring import PointsScoring
from ..valuation.blend import blend_recency, shrink_k, shrink_toward
from ..valuation.valuate import baseline_rates
from .data import SeasonTable, Windows, prev_season, season_label
from .fit import (InseasonObs, fit_inseason, fit_preseason, inseason_projection)
from .models import FmFitted, FmMulti, Model, get_models, group_means, make_context, prior_lines, prior_rows
from .scoring import fpg

MIN_GP = 20
MIN_GP_REST = 10
TOP_N = (50, 100)
INSEASON_MODELS = ("preseason", "to_date", "fm_shrink_only", "fm_current", "fm_fitted")


# --------------------------------------------------------------------------- metrics

def rankdata(xs: Sequence[float]) -> list[float]:
    """Ranks starting at 1, ties get their average rank."""
    order = sorted(range(len(xs)), key=lambda i: xs[i])
    ranks = [0.0] * len(xs)
    i = 0
    while i < len(order):
        j = i
        while j + 1 < len(order) and xs[order[j + 1]] == xs[order[i]]:
            j += 1
        avg = (i + j) / 2.0 + 1.0
        for t in range(i, j + 1):
            ranks[order[t]] = avg
        i = j + 1
    return ranks


def pearson(x: Sequence[float], y: Sequence[float]) -> float:
    n = len(x)
    if n < 2:
        return float("nan")
    mx, my = sum(x) / n, sum(y) / n
    sxy = sum((a - mx) * (b - my) for a, b in zip(x, y))
    sxx = sum((a - mx) ** 2 for a in x)
    syy = sum((b - my) ** 2 for b in y)
    return sxy / math.sqrt(sxx * syy) if sxx > 0 and syy > 0 else float("nan")


def spearman(x: Sequence[float], y: Sequence[float]) -> float:
    return pearson(rankdata(x), rankdata(y))


def top_n_precision(pred: Sequence[float], actual: Sequence[float], n: int) -> float | None:
    """|projected top n  intersect  actual top n| / n (None if fewer than n players)."""
    if len(pred) < n:
        return None
    idx = range(len(pred))
    top_p = set(sorted(idx, key=lambda i: -pred[i])[:n])
    top_a = set(sorted(idx, key=lambda i: -actual[i])[:n])
    return len(top_p & top_a) / n


@dataclass
class Metrics:
    n: int
    mae: float
    rmse: float
    bias: float
    spearman: float
    top50: float | None = None
    top100: float | None = None


def metrics(pred: Sequence[float], actual: Sequence[float], top: bool = True) -> Metrics:
    n = len(pred)
    if n == 0:
        nan = float("nan")
        return Metrics(0, nan, nan, nan, nan)
    err = [p - a for p, a in zip(pred, actual)]
    return Metrics(n=n, mae=sum(abs(e) for e in err) / n, rmse=math.sqrt(sum(e * e for e in err) / n),
                   bias=sum(err) / n, spearman=spearman(pred, actual),
                   top50=top_n_precision(pred, actual, 50) if top else None,
                   top100=top_n_precision(pred, actual, 100) if top else None)


@dataclass
class ResultRow:
    kind: str                  # preseason / inseason
    season: int
    checkpoint: str | None
    model: str
    pool: str                  # skaters / goalies
    n: int
    mae: float
    rmse: float
    bias: float
    spearman: float
    top50: float | None
    top100: float | None

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


def _row(kind: str, season: int, checkpoint: str | None, model: str, pool: str, m: Metrics) -> ResultRow:
    return ResultRow(kind, season, checkpoint, model, pool, m.n, m.mae, m.rmse, m.bias, m.spearman,
                     m.top50, m.top100)


# --------------------------------------------------------------------------- preseason

@dataclass
class Prediction:
    season: int
    player_id: int
    name: str
    group: str
    model: str
    projected: float
    actual: float


def preseason_predictions(table: SeasonTable, scoring: PointsScoring, season: int, models: Sequence[Model],
                          min_gp_prev: int = MIN_GP, min_gp: int = MIN_GP) -> list[Prediction]:
    ctx = make_context(table, season, scoring)
    prev = prev_season(season)
    out: list[Prediction] = []
    for pid, row in table.by_season.get(season, {}).items():
        last = table.get(pid, prev)
        if row.gp < min_gp or last is None or last.gp < min_gp_prev or last.group != row.group:
            continue
        history = table.history(pid, season)
        age = row.age if row.age is not None else table.age(pid, season)
        actual = fpg(row.per_game(), scoring)
        for m in models:
            r = m.rates(history, age, ctx)
            if r is None:
                continue
            out.append(Prediction(season, pid, row.name, row.group, m.name, fpg(r, scoring), actual))
    return out


def _pools(preds: Iterable[Prediction]) -> dict[tuple[str, str], list[Prediction]]:
    out: dict[tuple[str, str], list[Prediction]] = {}
    for p in preds:
        pool = "goalies" if p.group == "G" else "skaters"
        out.setdefault((p.model, pool), []).append(p)
    return out


def evaluate_preseason(table: SeasonTable, scoring: PointsScoring, seasons: Sequence[int],
                       model_names: Sequence[str] | None = None, min_gp_prev: int = MIN_GP,
                       min_gp: int = MIN_GP, log: Callable[[str], None] | None = None
                       ) -> tuple[list[ResultRow], list[Prediction]]:
    rows: list[ResultRow] = []
    all_preds: list[Prediction] = []
    first = min(table.seasons) if table.seasons else 0
    for n in seasons:
        models = get_models(model_names)
        if any(isinstance(m, (FmFitted, FmMulti)) for m in models):
            train = [s for s in table.seasons if prev_season(s) >= first and s <= prev_season(n)]
            params, _ = fit_preseason(table, scoring, train)
            for m in models:
                if isinstance(m, (FmFitted, FmMulti)):
                    m.params = params
        preds = preseason_predictions(table, scoring, n, models, min_gp_prev, min_gp)
        all_preds.extend(preds)
        for (model, pool), ps in sorted(_pools(preds).items()):
            rows.append(_row("preseason", n, None, model, pool,
                             metrics([p.projected for p in ps], [p.actual for p in ps], top=pool == "skaters")))
        if log:
            log(f"{season_label(n)}: {len({p.player_id for p in preds})} players projected")
    return rows, all_preds


# --------------------------------------------------------------------------- in-season

def inseason_observations(table: SeasonTable, windows: Windows, scoring: PointsScoring,
                          seasons: Sequence[int], min_gp_rest: int = MIN_GP_REST) -> list[InseasonObs]:
    """One observation per (checkpoint, player) with the app's own in-season FPG attached."""
    out: list[InseasonObs] = []
    for s in seasons:
        prev = prev_season(s)
        means = group_means(table, prev)
        for day, wins in sorted(windows.get(s, {}).items()):
            td, rest = wins.get("to_date", {}), wins.get("rest", {})
            for pid, cur in td.items():
                after = rest.get(pid)
                if after is None or after.gp < min_gp_rest or cur.gp < 1:
                    continue
                info = table.get(pid, s) or table.get(pid, prev)
                if info is None:
                    continue
                group = info.group
                rows = prior_rows(table.history(pid, s), s)
                latest = next((r for r in rows if r is not None), None)
                if latest is not None and latest.group == group:
                    # the app's preseason baseline: seasons N-1..N-3, fitted k, age factor
                    player = latest.to_player("prior")
                    player.lines = {ln.split: ln for ln in prior_lines(rows) if ln is not None}
                    base, _, _, _ = baseline_rates(player, means, table.age(pid, s))
                    has_base = bool(base)
                else:  # the app borrows the positional mean for a player with games but no baseline
                    base, has_base = dict(means.get(group, {})), bool(means.get(group))
                is_goalie = group == "G"
                cur_rates = cur.per_game()
                shrunk = shrink_toward(cur_rates, cur.gp, base, shrink_k(is_goalie)) if has_base else cur_rates
                lines = {w: wins.get(w, {}).get(pid) for w in ("last30", "last15", "last7")}
                app_rates = blend_recency(shrunk, *(ln.statline(w) if ln else None for w, ln in lines.items()))
                out.append(InseasonObs(
                    season=s, day=day, player_id=pid, group=group, gp_td=cur.gp,
                    fpg_td=fpg(cur_rates, scoring), fpg_base=fpg(base, scoring), has_base=has_base,
                    splits={w: (ln.gp, fpg(ln.per_game(), scoring)) if ln else (0, 0.0) for w, ln in lines.items()},
                    gp_rest=after.gp, fpg_rest=fpg(after.per_game(), scoring),
                    fpg_app=fpg(app_rates, scoring)))
    return out


def evaluate_inseason(obs: Sequence[InseasonObs], cv: bool = True
                      ) -> tuple[list[ResultRow], dict[int, dict[str, Any]]]:
    """Rows per (season, checkpoint, model, pool); fm_fitted is fitted leave-one-season-out."""
    seasons = sorted({o.season for o in obs})
    fits: dict[int, dict[str, Any]] = {}
    for s in seasons:
        train = [o for o in obs if o.season != s] if cv and len(seasons) > 1 else list(obs)
        fits[s] = fit_inseason(train)
    rows: list[ResultRow] = []
    groups: dict[tuple[int, str, str], list[InseasonObs]] = {}
    for o in obs:
        groups.setdefault((o.season, o.day, "goalies" if o.group == "G" else "skaters"), []).append(o)
    for (s, day, pool), os_ in sorted(groups.items()):
        f = fits[s]
        k_fit = f["k_goalie"] if pool == "goalies" else f["k_skater"]
        preds = {
            "preseason": [o.fpg_base if o.has_base else o.fpg_td for o in os_],
            "to_date": [o.fpg_td for o in os_],
            "fm_shrink_only": [inseason_projection(o, shrink_k(pool == "goalies"),
                                                   {"season": 1.0, "last30": 0.0, "last15": 0.0, "last7": 0.0})
                               for o in os_],
            "fm_current": [o.fpg_app for o in os_],
            "fm_fitted": [inseason_projection(o, k_fit, f["recency_weights"]) for o in os_],
        }
        actual = [o.fpg_rest for o in os_]
        for model in INSEASON_MODELS:
            rows.append(_row("inseason", s, day, model, pool, metrics(preds[model], actual, top=pool == "skaters")))
    return rows, fits


# --------------------------------------------------------------------------- summaries

def summarize(rows: Sequence[ResultRow], kind: str, pool: str) -> dict[str, dict[str, float]]:
    """model -> mean of each metric over seasons (and checkpoints)."""
    by: dict[str, list[ResultRow]] = {}
    for r in rows:
        if r.kind == kind and r.pool == pool:
            by.setdefault(r.model, []).append(r)
    out: dict[str, dict[str, float]] = {}
    for model, rs in by.items():
        def avg(attr: str) -> float | None:
            vals = [getattr(r, attr) for r in rs if getattr(r, attr) is not None and not math.isnan(getattr(r, attr))]
            return sum(vals) / len(vals) if vals else None
        out[model] = {a: avg(a) for a in ("mae", "rmse", "bias", "spearman", "top50", "top100")}
        out[model]["n"] = sum(r.n for r in rs)
        out[model]["seasons"] = len(rs)
    return out


def head_to_head(rows: Sequence[ResultRow], a: str, b: str, kind: str = "preseason", pool: str = "skaters",
                 metric: str = "mae") -> tuple[int, int]:
    """(seasons where a is better than b, seasons compared) on ``metric`` (lower MAE/RMSE,
    higher Spearman/top-N is better)."""
    lower = metric in ("mae", "rmse")
    idx: dict[tuple[int, str | None], dict[str, float]] = {}
    for r in rows:
        if r.kind == kind and r.pool == pool and r.model in (a, b):
            idx.setdefault((r.season, r.checkpoint), {})[r.model] = getattr(r, metric)
    wins = total = 0
    for v in idx.values():
        if a in v and b in v and v[a] is not None and v[b] is not None:
            total += 1
            wins += (v[a] < v[b]) if lower else (v[a] > v[b])
    return wins, total
