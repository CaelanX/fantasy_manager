"""Fill MoneyPuck expected-goals fields on a LeagueContext's skaters (and goalie GSAx).

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

Header guard: a season file missing a column a signal reads (``moneypuck.SKATER_XG_COLUMNS``,
``SKATER_ONICE_COLUMNS``, ``GOALIE_GSAX_COLUMNS``) skips that signal for that season with a
warning, instead of reading the column as 0 (ixG = 0 would drag every goal rate to 0).

Goalies (when the context has goalies with an NHL id and the client has ``goalies``): the
``all``-situation goalie files of the current season and the two before it (same cache TTLs) give
each goalie a regressed save talent, ``regression.regressed_gsax``: GSAx per shot over up to three
seasons weighted 1 / 0.8 / 0.6, with each season's xGA calibrated so league GSAx sums to ~0
(``moneypuck.goalie_calibration``). The estimates are registered per (current season start year,
NHL id) and read by ``valuation.valuate`` through ``goalie_gsax`` (reason GSAX).
"""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Iterable

from ..models import LeagueContext, Player
from ..valuation.regression import GSAX_SEASON_WEIGHTS, MIN_CURRENT_GP, GoalieGsax, regressed_gsax
from .moneypuck import (CORE_COLUMNS, CREDIT, GOALIE_GSAX_COLUMNS, SKATER_ONICE_COLUMNS, SKATER_XG_COLUMNS,
                        MoneyPuckClient, MpSkater, current_start_year, goalie_calibration, onice_sh_norms,
                        start_year)

# (current season start year, NHL id) -> regressed GSAx; replaced per season by each enrich_xg run
_GSAX: dict[tuple[int, int], GoalieGsax] = {}
# current season start year -> 5v5 on-ice shooting % norms of the last enrich_xg run
_NORMS: dict[int, dict[str, float]] = {}


def goalie_gsax(p: Player, season_start_year: int) -> GoalieGsax | None:
    """The registered GSAx estimate of goalie ``p`` for the season starting ``season_start_year``."""
    nid = p.nhl_id
    return _GSAX.get((int(season_start_year), nid)) if nid is not None else None


def onice_norms(season_start_year: int) -> dict[str, float]:
    """5v5 on-ice shooting % norms from the last enrich_xg run for that season ({} if none)."""
    return dict(_NORMS.get(int(season_start_year), {}))


def register_gsax(season_start_year: int, est: GoalieGsax) -> None:
    _GSAX[(int(season_start_year), int(est.nhl_id))] = est


def clear_registry() -> None:
    _GSAX.clear()
    _NORMS.clear()


@dataclass
class XgEnrichResult:
    seasons: tuple[int, int]                                  # (current, prior) start years
    filled: dict[str, int] = field(default_factory=dict)      # cid -> start year used
    from_prior: set[str] = field(default_factory=set)         # cids filled from the prior season
    small_sample: set[str] = field(default_factory=set)       # current season, < MIN_CURRENT_GP GP
    rows: dict[int, int] = field(default_factory=dict)        # start year -> skater rows (all)
    onice_sh_norms: dict[str, float] = field(default_factory=dict)  # 5v5 norms of the season used most
    errors: list[str] = field(default_factory=list)
    goalies: dict[str, GoalieGsax] = field(default_factory=dict)   # cid -> regressed GSAx
    goalie_errors: list[str] = field(default_factory=list)
    missing: list[str] = field(default_factory=list)               # header-guard warnings

    @property
    def count(self) -> int:
        return len(self.filled)


def _label(y: int) -> str:
    return f"{y}-{str(y + 1)[-2:]}"


def apply_xg(p: Player, row: MpSkater, five: MpSkater | None, xg_ok: bool = True, onice_ok: bool = True) -> None:
    """Fill the MoneyPuck fields; ``xg_ok`` / ``onice_ok`` False (a required column missing from
    the file) leave that signal's fields unset."""
    if xg_ok:
        p.ixg_per_game = round(row.ixg / row.gp, 4) if row.gp > 0 else None
        p.goals_minus_ixg = round(row.goals - row.ixg, 2)
    src = five if five is not None else row
    if onice_ok:
        p.onice_sh_pct = round(src.onice_sh_pct, 4) if src.onice_sh_pct is not None else None
    p.onice_xg_pct = src.onice_xg_pct


def _season_rows(client: MoneyPuckClient, year: int) -> tuple[dict[int, MpSkater], dict[int, MpSkater]]:
    rows = client.skaters(year, situation=None)
    return ({r.nhl_id: r for r in rows if r.situation == "all"},
            {r.nhl_id: r for r in rows if r.situation == "5on5"})


def _missing(client: Any, year: int, kind: str, required: tuple[str, ...]) -> list[str]:
    check = getattr(client, "missing", None)
    if check is None:          # a duck-typed client without raw files: nothing to check
        return []
    try:
        return list(check(year, kind, required))
    except Exception:
        return []


def _guard(ctx: LeagueContext, res: XgEnrichResult, client: Any, year: int, kind: str,
           required: tuple[str, ...], what: str) -> bool:
    """True when the season file has every ``required`` column; else a warning and False."""
    miss = _missing(client, year, kind, required)
    if not miss:
        return True
    msg = (f"MoneyPuck {_label(year)} {kind}.csv is missing column(s) {', '.join(miss)}: "
           f"{what} skipped for that season")
    res.missing.append(msg)
    if msg not in ctx.warnings:
        ctx.warnings.append(msg)
    return False


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

    pool = list(players if players is not None else ctx.all_players())
    _enrich_goalies(ctx, client, res, [p for p in pool if p.is_goalie and p.nhl_id is not None], cur_y)

    data: dict[int, tuple[dict[int, MpSkater], dict[int, MpSkater]]] = {}
    xg_ok: dict[int, bool] = {}
    onice_ok: dict[int, bool] = {}
    for y in (cur_y, prior_y):
        try:
            data[y] = _season_rows(client, y)
            res.rows[y] = len(data[y][0])
        except Exception as e:  # network / parse failure: keep going with what we have
            msg = f"MoneyPuck {_label(y)}: {type(e).__name__}: {str(e)[:120]}"
            res.errors.append(msg)
            ctx.warnings.append(msg)
            data[y] = ({}, {})
            continue
        if not _guard(ctx, res, client, y, "skaters", CORE_COLUMNS, "expected goals"):
            data[y] = ({}, {})
            continue
        xg_ok[y] = _guard(ctx, res, client, y, "skaters", SKATER_XG_COLUMNS, "the xG goal shrink (XG_SHRINK)")
        onice_ok[y] = _guard(ctx, res, client, y, "skaters", SKATER_ONICE_COLUMNS, "on-ice shooting % luck")
    if not any(data[y][0] for y in data):
        return res

    for p in pool:
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
        apply_xg(p, use, data[y][1].get(pid), xg_ok.get(y, True), onice_ok.get(y, True))
        p.xg_split = "prior" if p.cid in res.from_prior else "season"
        res.filled[p.cid] = y

    used = [y for y in (cur_y, prior_y) if data[y][1] and onice_ok.get(y, True)]
    if used:
        main = max(used, key=lambda y: sum(1 for v in res.filled.values() if v == y))
        res.onice_sh_norms = onice_sh_norms(data[main][1].values())
        if res.onice_sh_norms:
            _NORMS[cur_y] = dict(res.onice_sh_norms)
    if res.filled:
        n_cur = sum(1 for v in res.filled.values() if v == cur_y)
        parts = [f"{_label(cur_y)} for {n_cur} skaters"] if n_cur else []
        if res.from_prior:
            parts.append(f"{_label(prior_y)} for {len(res.from_prior)} with < {MIN_CURRENT_GP} GP in {_label(cur_y)}")
        note = f"{CREDIT} ({'; '.join(parts)})"
        ctx.source_notes.append(note)
    return res


def _enrich_goalies(ctx: LeagueContext, client: Any, res: XgEnrichResult, goalies: list[Player],
                    cur_y: int) -> None:
    """Regressed GSAx for ``goalies`` from the goalie files of ``cur_y`` and the seasons before it
    (one per ``GSAX_SEASON_WEIGHTS`` entry). Registered for valuation; failures are warnings."""
    for key in [k for k in _GSAX if k[0] == cur_y]:
        del _GSAX[key]
    if not goalies or not hasattr(client, "goalies"):
        return
    years = [cur_y - i for i in range(len(GSAX_SEASON_WEIGHTS))]
    by_year: dict[int, dict[int, tuple[Any, float, float | None]]] = {}
    for y in years:
        try:
            rows = client.goalies(y, situation="all")
        except Exception as e:  # network / parse failure: GSAx from the seasons we have
            msg = f"MoneyPuck goalies {_label(y)}: {type(e).__name__}: {str(e)[:120]}"
            res.goalie_errors.append(msg)
            ctx.warnings.append(msg)
            continue
        if not _guard(ctx, res, client, y, "goalies", CORE_COLUMNS + GOALIE_GSAX_COLUMNS, "goalie GSAx"):
            continue
        cal = goalie_calibration(rows)
        sa = sum(r.sa for r in rows)
        lsv = 1.0 - sum(r.ga for r in rows) / sa if sa > 0 else None
        by_year[y] = {r.nhl_id: (r, cal, lsv) for r in rows}
    if not any(by_year.values()):
        return
    for p in goalies:
        pid = int(p.nhl_id)  # type: ignore[arg-type]
        seasons = []
        for y in years:
            hit = by_year.get(y, {}).get(pid)
            if hit is None:
                seasons.append((y, 0.0, 0.0, 0.0, 0))
            else:
                r, cal, lsv = hit
                seasons.append((y, r.xga * cal, r.ga, r.sa, r.gp, lsv))
        est = regressed_gsax(pid, seasons)
        if est is None:
            continue
        register_gsax(cur_y, est)
        res.goalies[p.cid] = est
    if res.goalies:
        ctx.source_notes.append(f"{CREDIT}: goalie GSAx for {len(res.goalies)} goalies "
                                f"({', '.join(_label(y) for y in years if by_year.get(y))})")
