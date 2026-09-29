"""Nightly pulls of what actually happened: NHL stats, lineups and league transactions.

* ``pull_realized``      league-wide raw NHL stats per player for one game date (the stats REST
                         date-window reports via ``backtest.data.fetch_window``, faceoffs
                         included) into ``realized_daily``; scored at grade time with each
                         league's own ScoringConfig. A date without games stores nothing and
                         is recorded in ``realized_pulls`` with 0 players.
* ``pull_lineups``       ESPN box scores (one scoring period per day, with ESPN's points) or a
                         Fantrax roster snapshot into ``lineup_days``.
* ``pull_transactions``  provider activity since a date into ``transactions``; ``is_me`` marks my
                         own moves (and proposals involving me), other teams' moves are kept as
                         league-wide samples.

Every pull is an idempotent upsert and best effort: provider failures become warnings.
"""
from __future__ import annotations

import json
from datetime import date, timedelta
from typing import Any, Iterable

from ..models import ActivityItem, LeagueContext, LineupDay
from .ledger import Ledger, now_iso
from .match import set_my_team_id


def _my_team(ctx: LeagueContext | None) -> str | None:
    if ctx is None:
        return None
    try:
        return ctx.my_team.team_id
    except LookupError:
        return None


def pull_realized(ledger: Ledger, nhl_client: Any, day: date, season: int | None = None) -> int:
    """Store every player's raw stats for games on ``day``; returns players stored (0 = no games)."""
    from ..backtest.data import fetch_window
    from ..providers.nhl import current_season

    season = season or current_season(day)
    lines = fetch_window(nhl_client, season, day, day, faceoffs=True)
    now = now_iso()
    rows = [{"nhl_id": int(pid), "game_date": day.isoformat(), "gp": int(wl.gp),
             "stats_json": json.dumps(wl.stats, sort_keys=True), "pulled_at": now}
            for pid, wl in lines.items() if wl.gp > 0]
    ledger.upsert("realized_daily", rows, ("nhl_id", "game_date"))
    ledger.upsert("realized_pulls", [{"game_date": day.isoformat(), "n_players": len(rows), "pulled_at": now}],
                  ("game_date",))
    return len(rows)


def lineup_rows(league: str, items: Iterable[LineupDay], my_team: str | None) -> list[dict[str, Any]]:
    now = now_iso()
    return [{"league": league, "team_id": str(i.team_id), "day": i.date.isoformat(), "cid": i.cid, "slot": i.slot,
             "starting": int(bool(i.starting)), "provider_pts": i.provider_pts,
             "is_me": int(my_team is not None and str(i.team_id) == str(my_team)), "pulled_at": now}
            for i in items]


def pull_lineups(ledger: Ledger, provider: Any, league: str, day: date, ctx: LeagueContext | None = None,
                 today: date | None = None) -> int:
    """Lineups of every team for ``day``: ESPN box scores of that scoring period, Fantrax a
    snapshot of the loaded rosters labelled ``day``. Returns rows stored."""
    items: list[LineupDay] = []
    if hasattr(provider, "box_scores") and hasattr(provider, "scoring_period_for"):
        sp = provider.scoring_period_for(day, today)
        items = provider.box_scores(sp, day) if sp is not None else []
    elif hasattr(provider, "lineup_snapshot"):
        items = provider.lineup_snapshot(day)
    team = _my_team(ctx)
    return ledger.upsert("lineup_days", lineup_rows(league, items, team), ("league", "team_id", "day", "cid"))


def transaction_rows(league: str, items: Iterable[ActivityItem], my_team: str | None,
                     nhl_ids: dict[str, int] | None = None) -> list[dict[str, Any]]:
    now = now_iso()
    out = []
    for i in items:
        if not i.cid:
            continue
        mine = my_team is not None and (i.team_id == my_team or
                                        (i.action == "PROPOSED" and i.counterparty_id == my_team))
        out.append({"league": league, "source": i.source, "tx_id": i.tx_id, "action": i.action, "cid": i.cid,
                    "team_id": i.team_id or "", "ts": i.ts.isoformat(timespec="seconds"),
                    "day": i.ts.date().isoformat(), "team_name": i.team_name, "player_name": i.player_name,
                    "nhl_id": i.nhl_id if i.nhl_id is not None else (nhl_ids or {}).get(i.cid),
                    "group_id": i.group_id, "counterparty_id": i.counterparty_id, "is_me": int(mine),
                    "pulled_at": now})
    return out


def pull_transactions(ledger: Ledger, provider: Any, league: str, since: date | None,
                      ctx: LeagueContext | None = None) -> int:
    """Provider activity since ``since`` into ``transactions``; returns rows stored."""
    if not hasattr(provider, "activity"):
        return 0
    items = provider.activity(since)
    team = _my_team(ctx)
    if team is not None:
        set_my_team_id(ledger, league, team)
    nhl_ids = {p.cid: p.nhl_id for p in ctx.all_players() if p.nhl_id is not None} if ctx is not None else {}
    rows = transaction_rows(league, items, team, nhl_ids)
    # a pending proposal has no reliable timestamp: keep the time it was first seen
    seen = {(r["tx_id"], r["cid"], r["team_id"]): (r["ts"], r["day"]) for r in ledger.query(
        "SELECT tx_id, cid, team_id, ts, day FROM transactions WHERE league=? AND action='PROPOSED'", (league,))}
    for r in rows:
        old = seen.get((r["tx_id"], r["cid"], r["team_id"])) if r["action"] == "PROPOSED" else None
        if old is not None:
            r["ts"], r["day"] = old
    return ledger.upsert("transactions", rows, ("league", "tx_id", "action", "cid", "team_id"))


def yesterday(today: date | None = None) -> date:
    return (today or date.today()) - timedelta(days=1)
