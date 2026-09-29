"""`fm advise`: run every available recommender and merge into one ranked list.

Each recommender is imported and called defensively (a missing module or a failure in one
engine never hides the others). Raw scores are not comparable across engines, so each kind
group is rescaled by rank to 0-10 (best = 10, then linearly down to 10/n), and the merged
list is ordered by that normalized score with ties broken by kind priority:
injury > lineup > waiver > trade > flags (sell_high / buy_low). Each rec also carries its
absolute ``strength`` (recommend.strength, comparable across kinds) and ``rank_in_kind`` /
``kind_total`` ("1 of 4 waivers").
"""
from __future__ import annotations

import importlib
import inspect
import logging
from typing import Any, Callable, Iterable, Mapping

from ..models import LeagueContext, Reason, Recommendation
from .strength import strength_for

log = logging.getLogger(__name__)

PRIORITY = ("injury", "lineup", "waiver", "trade", "flags")
KIND_GROUP = {"injury": "injury", "lineup": "lineup", "waiver": "waiver", "trade": "trade",
              "sell_high": "flags", "buy_low": "flags"}
# include-name -> (module, candidate function names)
ENGINES: dict[str, tuple[str, tuple[str, ...]]] = {
    "injuries": ("injuries", ("recommend_injuries", "injury_recommendations", "recommend")),
    "lineup": ("lineup", ("recommend_lineup", "lineup_recommendations", "recommend")),
    "waivers": ("waivers", ("recommend_waivers",)),
    "trades": ("trades", ("recommend_trades",)),
    "flags": ("flags", ("recommend_flags",)),
}


def _resolve(include_name: str) -> Callable[..., Any] | None:
    spec = ENGINES.get(include_name)
    if spec is None:
        return None
    mod_name, fn_names = spec
    try:
        mod = importlib.import_module(f"{__package__}.{mod_name}")
    except Exception as e:  # module not written yet / import error
        log.info("advise: skipping %s (%s)", include_name, e)
        return None
    for n in fn_names:
        fn = getattr(mod, n, None)
        if callable(fn):
            return fn
    return None


def _call(fn: Callable[..., Any], ctx: LeagueContext, values: Mapping[str, Any],
          **optional: Any) -> list[Recommendation]:
    """Call fn(ctx, values, ...) passing only the optional kwargs it declares."""
    try:
        params = inspect.signature(fn).parameters
        takes_kwargs = any(p.kind is inspect.Parameter.VAR_KEYWORD for p in params.values())
        kw = {k: v for k, v in optional.items()
              if k in params or (takes_kwargs and v is not None)}
    except (TypeError, ValueError):
        kw = {}
    out = fn(ctx, values, **kw)
    return [r for r in (out or []) if isinstance(r, Recommendation)]


def normalize_scores(recs: Iterable[Recommendation]) -> list[Recommendation]:
    """Rank-based 0-10 rescale within each kind group; raw score kept as a RAW_SCORE reason."""
    groups = group_by_kind(recs, by_group=True)
    out: list[Recommendation] = []
    for items in groups.values():
        ranked = sorted(items, key=lambda r: r.score, reverse=True)
        n = len(ranked)
        for i, r in enumerate(ranked):
            norm = 10.0 * (n - i) / n
            out.append(r.model_copy(update={
                "score": round(norm, 4),
                "strength": r.strength if r.strength is not None else _strength(r),
                "rank_in_kind": i + 1,
                "kind_total": n,
                "reasons": list(r.reasons) + [Reason(code="RAW_SCORE",
                                                     text=f"{r.kind} engine score {r.score:.3f} "
                                                          f"(rank {i + 1}/{n})", value=r.score)],
            }))
    return out


def _strength(r: Recommendation) -> float | None:
    try:
        return strength_for(r)
    except Exception:  # display field only
        return None


def _priority(r: Recommendation) -> int:
    g = KIND_GROUP.get(r.kind, "flags")
    return PRIORITY.index(g) if g in PRIORITY else len(PRIORITY)


def advise(ctx: LeagueContext, values: Mapping[str, Any],
           dynasty_values: Mapping[str, Any] | None = None, history: Any = None,
           include: Iterable[str] = ("lineup", "waivers", "trades", "flags", "injuries"),
           limit: int | None = None) -> list[Recommendation]:
    """Merged, normalized and ranked recommendations from every available engine."""
    raw: list[Recommendation] = []
    for name in include:
        fn = _resolve(name)
        if fn is None:
            continue
        try:
            raw.extend(_call(fn, ctx, values, dynasty_values=dynasty_values, history=history))
        except Exception as e:
            log.warning("advise: %s failed: %s", name, e)
    merged = normalize_scores(raw)
    merged.sort(key=lambda r: (-r.score, _priority(r)))
    return merged[:limit] if limit else merged


def group_by_kind(recs: Iterable[Recommendation], by_group: bool = False
                  ) -> dict[str, list[Recommendation]]:
    """Recommendations bucketed by kind (or by group, merging sell_high/buy_low into
    'flags'), buckets in kind-priority order, order within a bucket preserved."""
    out: dict[str, list[Recommendation]] = {}
    for r in sorted(recs, key=_priority):  # stable: keeps incoming order within a kind
        key = KIND_GROUP.get(r.kind, r.kind) if by_group else r.kind
        out.setdefault(key, []).append(r)
    return out
