"""Rookie evidence for the enrich pipeline: league history (NHLe input) and news role signals.

:func:`enrich_rookies` (called by ``providers.enrich`` as step 6c, best effort):

1. League history for the context's unproven skaters with NHL ids
   (``preseason_enrich.is_unproven``): ``NhlClient.player_history`` from the player landing page.
   The pedigree step (6b) already fetched that page for young / unproven players, so its parsed
   landings are reused (``landings``); only the remaining players are fetched, through the 30-day
   HTTP cache, at most ``limit`` (200) uncached pages per run (the rest fill in on later runs).
2. News role signals (``providers.news_roles.extract_role_signals``) over the RotoWire / ESPN
   items (``news``, or fetched here through the cache: the same 30-minute RSS entries the news
   commands read), mapped to league players with ``report.news_match``: a RotoWire item goes to its
   named player; an ESPN headline only when it names exactly one league player. The optional LLM
   pass runs when ``llm`` is available (cache ``<fm_data_dir>/news_roles.json``).

``models.Player`` / ``LeagueContext`` are not extended here, so results live in module registries
(like ``lines_enrich`` / ``preseason_enrich``): history by NHL id, signals by player cid.
:func:`history_for` / :func:`signals_for` read them back for ``valuation.valuate`` (the rookie
model), ``recommend.flags`` (rookie role alerts) and the player page.
"""
from __future__ import annotations

import logging
from typing import Any, Callable, Iterable, Mapping

from ..models import LeagueContext, Player
from .news import NewsItem
from .news_roles import RoleSignal, extract_role_signals
from .nhl import LeagueSeason, NhlClient, parse_season_totals
from .preseason_enrich import is_unproven

log = logging.getLogger(__name__)

HISTORY_LIMIT = 200

_HISTORY: dict[int, list[LeagueSeason]] = {}
_SIGNALS: dict[str, list[RoleSignal]] = {}


# --------------------------------------------------------------------------- registry

def register_history(nhl_id: int, rows: Iterable[LeagueSeason]) -> None:
    _HISTORY[int(nhl_id)] = list(rows)


def register_signals(cid: str, signals: Iterable[RoleSignal]) -> None:
    _SIGNALS[cid] = list(signals)


def clear_registry() -> None:
    _HISTORY.clear()
    _SIGNALS.clear()


def history_for(p: Player) -> list[LeagueSeason]:
    nid = p.nhl_id
    return list(_HISTORY.get(nid, [])) if nid is not None else []


def signals_for(p: Player) -> list[RoleSignal]:
    return list(_SIGNALS.get(p.cid, []))


def all_signals() -> dict[str, list[RoleSignal]]:
    return {k: list(v) for k, v in _SIGNALS.items()}


# --------------------------------------------------------------------------- steps

def _targets(ctx: LeagueContext) -> list[Player]:
    rostered = {p.cid for t in ctx.teams for p in t.players}
    todo = [p for p in ctx.all_players() if p.nhl_id is not None and not p.is_goalie and is_unproven(p)]
    todo.sort(key=lambda p: (p.cid not in rostered, -(p.pct_owned or 0.0)))
    return todo


def load_histories(ctx: LeagueContext, client: NhlClient | None, landings: Mapping[int, Any] | None = None, *,
                   limit: int = HISTORY_LIMIT, is_cached: Callable[[int], bool] | None = None,
                   fetch_missing: bool = True) -> dict[str, int]:
    """Register league history for the unproven skaters (see the module docstring)."""
    res = {"targets": 0, "reused": 0, "fetched": 0, "cached": 0, "failed": 0, "deferred": 0}
    landings = landings or {}
    for p in _targets(ctx):
        nid = int(p.nhl_id)  # type: ignore[arg-type]
        res["targets"] += 1
        landing = landings.get(nid)
        if landing is not None:
            register_history(nid, parse_season_totals(getattr(landing, "season_totals", None),
                                                      getattr(landing, "birth_date", None)))
            res["reused"] += 1
            continue
        if nid in _HISTORY:
            res["cached"] += 1
            continue
        if client is None or not fetch_missing:
            continue
        cached = bool(is_cached and is_cached(nid))
        if not cached:
            if res["fetched"] >= limit:
                res["deferred"] += 1
                continue
            res["fetched"] += 1
        else:
            res["cached"] += 1
        try:
            register_history(nid, client.player_history(nid))
        except Exception:
            res["failed"] += 1
    return res


def attach_signals(ctx: LeagueContext, news: Iterable[NewsItem], signals: Iterable[RoleSignal]) -> int:
    """Map signals to league players (cid) through their news items; returns players with signals."""
    from ..report.news_match import match_news_to_players

    news = list(news)
    players = ctx.all_players()
    by_cid = match_news_to_players(news, players)
    item_owners: dict[str, list[str]] = {}
    for cid, items in by_cid.items():
        for it in items:
            key = it.id or f"{it.source}|{it.headline}|{it.published}"
            item_owners.setdefault(key, []).append(cid)
    out: dict[str, list[RoleSignal]] = {}
    for s in signals:
        owners = item_owners.get(s.item_id or "", [])
        if len(owners) != 1:                 # unnamed or naming several league players: skip
            continue
        out.setdefault(owners[0], []).append(s.model_copy(update={"cid": owners[0]}))
    for cid, sigs in out.items():
        register_signals(cid, sigs)
    return len(out)


def fetch_news(fetch_text: Callable[[str, "dict | None"], str], warnings: list[str] | None = None) -> list[NewsItem]:
    """RotoWire + ESPN items through ``fetch_text`` (a failing source becomes a warning)."""
    from .news import fetch_espn_news, fetch_rotowire

    items: list[NewsItem] = []
    for label, fn in (("RotoWire news", fetch_rotowire), ("ESPN news", fetch_espn_news)):
        try:
            items.extend(fn(fetch_text))
        except Exception as e:
            if warnings is not None:
                warnings.append(f"Rookie role signals: {label} unavailable ({type(e).__name__})")
    return items


ITEMS_NAME = "news_role_items.json"
ITEMS_MAX_AGE_DAYS = 30
ITEMS_MAX = 500


def remembered_items(store_dir: Any, as_of: Any = None) -> list[NewsItem]:
    """News items that produced role signals on earlier runs (``<store_dir>/news_role_items.json``,
    <= 30 days old): the RSS feeds only carry the latest few dozen items, so a "named to the
    opening-night roster" blurb would otherwise be forgotten within hours."""
    import json
    from datetime import datetime, timedelta, timezone
    from pathlib import Path

    if store_dir is None:
        return []
    try:
        raw = json.loads((Path(store_dir) / ITEMS_NAME).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return []
    ref = datetime.now(timezone.utc) if as_of is None else         datetime(as_of.year, as_of.month, as_of.day, tzinfo=timezone.utc) + timedelta(days=1)
    out = []
    for d in raw.values() if isinstance(raw, dict) else []:
        try:
            it = NewsItem.model_validate(d)
        except Exception:
            continue
        if it.published is not None and (ref - it.published).days > ITEMS_MAX_AGE_DAYS:
            continue
        out.append(it)
    return out


def remember_items(store_dir: Any, items: Iterable[NewsItem]) -> None:
    """Merge ``items`` into ``<store_dir>/news_role_items.json`` (newest ITEMS_MAX kept)."""
    import json
    from datetime import datetime, timezone
    from pathlib import Path

    if store_dir is None:
        return
    path = Path(store_dir) / ITEMS_NAME
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
        raw = raw if isinstance(raw, dict) else {}
    except (OSError, ValueError):
        raw = {}
    for it in items:
        if it.id:
            raw[it.id] = it.model_dump(mode="json")
    epoch = datetime.min.replace(tzinfo=timezone.utc).isoformat()
    keep = sorted(raw.items(), key=lambda kv: str(kv[1].get("published") or epoch), reverse=True)[:ITEMS_MAX]
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(dict(keep), indent=1, sort_keys=True), encoding="utf-8")
        tmp.replace(path)
    except OSError as e:
        log.info("news role items not saved: %s", e)


def enrich_rookies(ctx: LeagueContext, client: NhlClient | None, *, landings: Mapping[int, Any] | None = None,
                   news: list[NewsItem] | None = None, fetch_text: Callable | None = None, llm: Any = None,
                   store_dir: Any = None, limit: int = HISTORY_LIMIT, is_cached: Callable[[int], bool] | None = None,
                   fetch_missing: bool = True) -> dict[str, Any]:
    """Steps 1-2 of the module docstring; adds one source note. Returns counts."""
    res: dict[str, Any] = load_histories(ctx, client, landings, limit=limit, is_cached=is_cached,
                                         fetch_missing=fetch_missing)
    if news is None and fetch_text is not None:
        news = fetch_news(fetch_text, ctx.warnings)
    fresh = list(news or [])
    seen = {it.id for it in fresh if it.id}
    news = fresh + [it for it in remembered_items(store_dir, ctx.as_of) if it.id not in seen]
    signals: list[RoleSignal] = []
    if news:
        signals = extract_role_signals(news, llm=llm, store_dir=store_dir)
        res["signal_players"] = attach_signals(ctx, news, signals)
        with_signals = {s.item_id for s in signals}
        remember_items(store_dir, [it for it in fresh if it.id in with_signals])
    res["news"] = len(news or [])
    res["signals"] = len(signals)
    res["llm"] = sum(1 for s in signals if s.origin == "llm")
    with_hist = sum(1 for p in _targets(ctx) if history_for(p))
    deferred = f", {res['deferred']} deferred (limit {limit})" if res.get("deferred") else ""
    failed = f", {res['failed']} failed" if res.get("failed") else ""
    llm_note = f", {res['llm']} from the LLM pass" if res["llm"] else ""
    ctx.source_notes.append(
        f"Rookie evidence: league history for {with_hist}/{res['targets']} unproven skaters (NHL player pages, "
        f"{res['reused']} reused from the pedigree step{failed}{deferred}); {res['signals']} news role signals from "
        f"{res['news']} items for {res.get('signal_players', 0)} league players{llm_note} (approximate NHLe factors)")
    return res


__all__ = ["enrich_rookies", "history_for", "signals_for", "register_history", "register_signals",
           "clear_registry", "attach_signals", "load_histories", "all_signals"]
