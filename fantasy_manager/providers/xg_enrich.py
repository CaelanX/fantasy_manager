"""Fill MoneyPuck expected-goals fields on a LeagueContext's skaters.

For every skater with an NHL id (``player.ids["nhl"]``, set by the crosswalk) the season used is
the current one once he has ``MIN_CURRENT_GP`` (5) MoneyPuck games, else the prior season
(flagged: ``XgEnrichResult.from_prior`` and a source note), else a small current sample:

* ``ixg_per_game``    = I_F_xGoals / games_played          (situation ``all``)
* ``goals_minus_ixg`` = I_F_goals - I_F_xGoals             (situation ``all``, season total)
* ``onice_sh_pct``    = OnIce_F_goals / OnIce_F_shotsOnGoal (situation ``5on5``: the usual
  PDO-style luck measure; all-situation values are inflated for power-play regulars)
* ``onice_xg_pct``    = onIce_xGoalsPercentage             (situation ``5on5``, 0..1)
* ``xg_split``        = "season" or "prior": which season the fields above describe

``valuation.regression.xg_sample`` infers the same season choice from the player's NHL lines.
Best effort: a download failure adds a warning to ``ctx.warnings`` and leaves players untouched.
"""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Iterable

from ..models import LeagueContext, Player
from ..valuation.regression import MIN_CURRENT_GP
from .moneypuck import CREDIT, MoneyPuckClient, MpSkater, current_start_year, onice_sh_norms, start_year


@dataclass
class XgEnrichResult:
    seasons: tuple[int, int]                                  # (current, prior) start years
    filled: dict[str, int] = field(default_factory=dict)      # cid -> start year used
    from_prior: set[str] = field(default_factory=set)         # cids filled from the prior season
    small_sample: set[str] = field(default_factory=set)       # current season, < MIN_CURRENT_GP GP
    rows: dict[int, int] = field(default_factory=dict)        # start year -> skater rows (all)
    onice_sh_norms: dict[str, float] = field(default_factory=dict)  # 5v5 norms of the season used most
    errors: list[str] = field(default_factory=list)

    @property
    def count(self) -> int:
        return len(self.filled)


def _label(y: int) -> str:
    return f"{y}-{str(y + 1)[-2:]}"


def apply_xg(p: Player, row: MpSkater, five: MpSkater | None) -> None:
    p.ixg_per_game = round(row.ixg / row.gp, 4) if row.gp > 0 else None
    p.goals_minus_ixg = round(row.goals - row.ixg, 2)
    src = five if five is not None else row
    p.onice_sh_pct = round(src.onice_sh_pct, 4) if src.onice_sh_pct is not None else None
    p.onice_xg_pct = src.onice_xg_pct


def _season_rows(client: MoneyPuckClient, year: int) -> tuple[dict[int, MpSkater], dict[int, MpSkater]]:
    rows = client.skaters(year, situation=None)
    return ({r.nhl_id: r for r in rows if r.situation == "all"},
            {r.nhl_id: r for r in rows if r.situation == "5on5"})


def enrich_xg(ctx: LeagueContext, cache: Any = None, seasons: Iterable[int] | None = None,
              client: MoneyPuckClient | None = None, players: Iterable[Player] | None = None
              ) -> XgEnrichResult:
    """Fill ixG fields for ``players`` (default ``ctx.all_players()``). ``seasons`` = (current,
    prior) as start years or NHL season ids (default: from ``ctx.as_of``). ``client`` defaults to
    a MoneyPuckClient reading through ``cache`` (12 h / 30 d TTLs)."""
    if seasons is None:
        cur = current_start_year(ctx.as_of)
        seasons = (cur, cur - 1)
    yrs = [start_year(s) for s in seasons]
    cur_y, prior_y = yrs[0], (yrs[1] if len(yrs) > 1 else yrs[0] - 1)
    res = XgEnrichResult(seasons=(cur_y, prior_y))
    if client is None:
        client = MoneyPuckClient.from_cache(cache) if cache is not None else MoneyPuckClient()

    data: dict[int, tuple[dict[int, MpSkater], dict[int, MpSkater]]] = {}
    for y in (cur_y, prior_y):
        try:
            data[y] = _season_rows(client, y)
            res.rows[y] = len(data[y][0])
        except Exception as e:  # network / parse failure: keep going with what we have
            msg = f"MoneyPuck {_label(y)}: {type(e).__name__}: {str(e)[:120]}"
            res.errors.append(msg)
            ctx.warnings.append(msg)
            data[y] = ({}, {})
    if not any(data[y][0] for y in data):
        return res

    for p in (players if players is not None else ctx.all_players()):
        pid = p.nhl_id
        if pid is None or p.is_goalie:
            continue
        cur = data[cur_y][0].get(pid)
        prior = data[prior_y][0].get(pid)
        if cur is not None and cur.gp >= MIN_CURRENT_GP:
            use, y = cur, cur_y
        elif prior is not None and prior.gp > 0:
            use, y = prior, prior_y
            res.from_prior.add(p.cid)
        elif cur is not None and cur.gp > 0:
            use, y = cur, cur_y
            res.small_sample.add(p.cid)
        else:
            continue
        apply_xg(p, use, data[y][1].get(pid))
        p.xg_split = "prior" if p.cid in res.from_prior else "season"
        res.filled[p.cid] = y

    used = [y for y in (cur_y, prior_y) if data[y][1]]
    if used:
        main = max(used, key=lambda y: sum(1 for v in res.filled.values() if v == y))
        res.onice_sh_norms = onice_sh_norms(data[main][1].values())
    if res.filled:
        n_cur = sum(1 for v in res.filled.values() if v == cur_y)
        parts = [f"{_label(cur_y)} for {n_cur} skaters"] if n_cur else []
        if res.from_prior:
            parts.append(f"{_label(prior_y)} for {len(res.from_prior)} with < {MIN_CURRENT_GP} GP in {_label(cur_y)}")
        note = f"{CREDIT} ({'; '.join(parts)})"
        ctx.source_notes.append(note)
    return res
