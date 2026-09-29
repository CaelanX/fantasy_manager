"""`fm` command-line interface."""
from __future__ import annotations

import json
import logging
import sys
from dataclasses import dataclass, field
from datetime import datetime, timezone
from enum import Enum
from pathlib import Path
from typing import Any, Callable, Optional

import typer
from rich.console import Console
from rich.markup import escape
from rich.table import Table

from .cache import CacheMiss, HttpCache
from .cli_backtest import backtest_app
from .cli_harness import harness_app
from .config import get_settings
from .models import LeagueContext, Recommendation
from .providers import ProviderUnavailable, get_provider
from .providers.base import ProviderError
from .scoring import from_config
from .valuation.valuate import PlayerValue, replacement_for, valuate_league

app = typer.Typer(help="Fantasy hockey manager: roster values, lineups, waiver pickups, injuries and more.",
                  no_args_is_help=True, add_completion=False)
app.add_typer(backtest_app, name="backtest")
app.add_typer(harness_app, name="harness")
console = Console(emoji=False)
err = Console(stderr=True, emoji=False)

SLOT_ORDER = {s: i for i, s in enumerate(("C", "LW", "RW", "F", "D", "UTIL", "G", "BN", "IR"))}


class LeagueName(str, Enum):
    espn = "espn"
    fantrax = "fantrax"


class HorizonName(str, Enum):
    week = "week"
    season = "season"


class ModeName(str, Enum):
    contend = "contend"
    balanced = "balanced"
    rebuild = "rebuild"


def _mode_opt() -> Any:
    return typer.Option(None, "--mode", help="Dynasty mode for this run only (not saved; `fm mode` saves one).")


def _mode_val(mode: Optional[ModeName]) -> str | None:
    return mode.value if mode else None


@app.callback()
def main(ctx: typer.Context,
         league: LeagueName = typer.Option(LeagueName.espn, "--league", "-l", help="Which league to use."),
         json_out: bool = typer.Option(False, "--json", help="Print JSON instead of tables.")) -> None:
    ctx.obj = {"league": league.value, "json": json_out}
    # Windows consoles / redirected logs are often cp1252: never crash on a player's name
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.reconfigure(errors="replace")  # type: ignore[union-attr]
        except Exception:
            pass


# -- helpers ----------------------------------------------------------------

def _opts(ctx: typer.Context, league: Optional[LeagueName], json_out: bool) -> tuple[str, bool]:
    obj = ctx.obj or {}
    return (league.value if league else obj.get("league", "espn")), (json_out or obj.get("json", False))


def _fail(msg: str, code: int = 1) -> None:
    err.print(f"[bold red]Error:[/] {escape(msg)}")
    raise typer.Exit(code)


def _cache() -> HttpCache:
    s = get_settings()
    return HttpCache(s.fm_data_dir, offline=s.fm_offline)


def _resolve_mode(lc: LeagueContext, override: str | None = None) -> None:
    """Set the effective dynasty mode on the context: ``--mode`` > prefs.json (dashboard /
    `fm mode`) > FANTRAX_MODE > default, recording where it came from."""
    if override:
        lc.dynasty_mode, lc.dynasty_mode_source = override, "option"  # type: ignore[assignment]
        return
    from .prefs import dynasty_mode_info

    try:
        mode, source = dynasty_mode_info(get_settings())
    except Exception as e:  # a broken prefs file must never sink a command
        lc.warnings.append(f"Could not resolve the dynasty mode: {e}")
        return
    lc.dynasty_mode, lc.dynasty_mode_source = mode, source  # type: ignore[assignment]


def _mode_line(lc: LeagueContext) -> str:
    from .prefs import mode_source_label

    src = f" (from {mode_source_label(lc.dynasty_mode_source)})" if lc.dynasty_mode_source else ""
    return f"Dynasty mode: {lc.dynasty_mode}{src}"


def _load_context(league: str, cache: HttpCache | None = None, mode: str | None = None
                  ) -> tuple[LeagueContext, Any]:
    """provider.load() with user-facing errors and the effective dynasty mode applied
    (``mode`` overrides it for this run). Returns (ctx, provider)."""
    settings = get_settings()
    try:
        cache = cache or _cache()
        provider = get_provider(league, settings, cache)
        lc = provider.load()
    except ProviderUnavailable as e:
        err.print(f"[yellow]{e}[/]")
        raise typer.Exit(2)
    except ProviderError as e:
        _fail(str(e))
    except CacheMiss as e:
        _fail(f"offline mode (FM_OFFLINE=1) and data not cached: {e}")
    lc.warnings.extend(str(w) for w in getattr(provider, "warnings", None) or [])
    _resolve_mode(lc, mode)
    return lc, provider


def _valuate(lc: LeagueContext) -> dict[str, PlayerValue]:
    """Value every player. Categories/roto systems are fitted on the rostered pool inside
    valuate_league, so values are z-score sums rather than all zero."""
    if lc.scoring.kind != "points":
        cats = ", ".join(lc.scoring.categories) or "none configured"
        lc.source_notes.append(f"{lc.scoring.kind} league: FPG/VORP columns are per-game z-score value "
                               f"summed over categories ({cats}), not fantasy points")
    return valuate_league(lc, from_config(lc.scoring))


def _dynasty(lc: LeagueContext, values: dict[str, PlayerValue], provider: Any = None
             ) -> dict[str, Any] | None:
    if not lc.dynasty:
        return None
    try:
        from .valuation.dynasty import apply_dynasty
    except (ImportError, AttributeError, SyntaxError):
        lc.warnings.append("Dynasty valuation is not available yet.")
        return None
    # FantraxProvider.ages ({cid: age}) fills in players without a birth date
    ages = getattr(provider, "ages", None)
    try:
        if isinstance(ages, dict) and ages:
            return apply_dynasty(values, lc, ages=ages)
        return apply_dynasty(values, lc)
    except Exception as e:  # never let an optional column sink the command
        lc.warnings.append(f"Dynasty valuation failed: {e}")
        return None


@dataclass
class Loaded:
    """Everything a command needs: context, values, dynasty values and the provider."""
    lc: LeagueContext
    values: dict[str, PlayerValue]
    dyn: dict[str, Any] | None
    provider: Any
    cache: HttpCache
    errors: list[str] = field(default_factory=list)


def _load_all(league: str, deep: bool = False, cache: HttpCache | None = None,
              mode: str | None = None) -> Loaded:
    """provider.load -> enrich (NHL ids, schedule, injuries, stats) -> valuate -> dynasty.
    ``mode`` overrides the dynasty mode for this run only (never saved)."""
    from .providers.enrich import enrich_context

    cache = cache or _cache()
    lc, provider = _load_context(league, cache, mode)
    try:
        enrich_context(lc, get_settings(), cache, deep=deep)
    except Exception as e:  # enrichment is best effort
        lc.warnings.append(f"NHL enrichment failed: {e}")
    values = _valuate(lc)
    return Loaded(lc, values, _dynasty(lc, values, provider), provider, cache)


def _load(league: str, deep: bool = False, cache: HttpCache | None = None, mode: str | None = None
          ) -> tuple[LeagueContext, dict[str, PlayerValue], dict[str, Any] | None]:
    """(ctx, values, dynasty values); see _load_all."""
    d = _load_all(league, deep, cache, mode)
    return d.lc, d.values, d.dyn


def _dump(data: Any) -> None:
    typer.echo(json.dumps(data, indent=2, default=str))


def _meta(lc: LeagueContext) -> dict[str, Any]:
    return {"provider": lc.provider, "league_id": lc.league_id, "as_of": lc.as_of.isoformat(),
            "season_start": lc.season_start.isoformat() if lc.season_start else None,
            "sources": lc.source_notes, "warnings": lc.warnings}


def _footer(lc: LeagueContext, errors: list[str] | None = None) -> None:
    if lc.dynasty:
        console.print(f"[dim]{escape(_mode_line(lc))}; {lc.keeper_horizon_years}-year horizon "
                      "(change with `fm mode contend|balanced|rebuild` or the dashboard toggle)[/]")
    for n in lc.source_notes:
        console.print(f"[dim]{n}[/]")
    for w in [*lc.warnings, *(errors or [])]:
        console.print(f"[dim yellow]! {w}[/]")


def _params_source() -> str:
    try:
        from .valuation.params import source

        return source()
    except Exception as e:  # noqa: BLE001
        return f"unavailable ({type(e).__name__})"


def _fmt(v: float | None, signed: bool = False) -> str:
    if v is None:
        return "-"
    return f"{v:+.2f}" if signed else f"{v:.2f}"


def _pos(p: Any) -> str:
    return "/".join(x for x in p.positions if x != "F") or "/".join(p.positions)


PROSPECT_MAX_AGE = 22
PROSPECT_MAX_GP = 82


def _age(p: Any, lc: LeagueContext, dyn: dict[str, Any] | None) -> float | None:
    a = getattr((dyn or {}).get(p.cid), "age", None)
    if isinstance(a, (int, float)):
        return float(a)
    bd = getattr(p, "birth_date", None)
    return (lc.as_of - bd).days / 365.25 if bd else None


def _is_prospect(p: Any, age: float | None) -> bool:
    """Age <= 22 with fewer than 82 career NHL games (unknown career GP counts as few only
    when the player has no NHL stat line this or last season)."""
    if age is None or age >= PROSPECT_MAX_AGE + 1:
        return False
    gp = getattr(p, "career_gp", None)
    if gp is None:
        gp = p.gp("season") + p.gp("prior")
    return gp < PROSPECT_MAX_GP


def _dyn_value(d: Any) -> float | None:
    v = getattr(d, "value", None)
    return float(v) if isinstance(v, (int, float)) else None


def _dyn_json(d: Any) -> Any:
    if d is None:
        return None
    if hasattr(d, "model_dump"):
        return d.model_dump(mode="json", exclude={"player"})
    return {k: getattr(d, k, None) for k in ("value", "age", "age_mult", "upside", "reasons")}


def _strength_cell(r: Recommendation) -> str:
    return "-" if r.strength is None else f"{r.strength:.1f}"


def _rank_cell(r: Recommendation, i: int) -> str:
    from .recommend.strength import rank_text

    txt = rank_text(r)
    return str(i) if txt == "-" else txt


def _gain_cell(r: Recommendation) -> str:
    from .recommend.strength import gain_text

    return gain_text(r)


def _rec_table(recs: list[Recommendation], title: str) -> Table:
    t = Table(title=title, show_lines=True)
    for c, j in (("#", "right"), ("Recommendation", "left"), ("Strength", "right"), ("Gain", "right"),
                 ("Why", "left")):
        t.add_column(c, justify=j)
    for i, r in enumerate(recs, 1):
        why = "\n".join(x.text for x in r.reasons if x.code != "RAW_SCORE")
        if r.narrative:
            why = f"[italic]{escape(r.narrative)}[/]\n{why}"
        t.add_row(_rank_cell(r, i), r.title, _strength_cell(r), _gain_cell(r), why)
    return t


STATUS_STYLE = {"healthy": "green", "dtd": "yellow", "out": "red", "ir": "red", "ltir": "red",
                "suspended": "magenta", "unknown": "dim"}


def _status(p: Any) -> str:
    return f"[{STATUS_STYLE.get(p.status, '')}]{p.status}[/]"


# -- commands ---------------------------------------------------------------

def _print_fit(lc: LeagueContext, provider: Any, as_json: bool) -> None:
    """`settings --fit-points`: least-squares point values from the platform's own FPts."""
    import math

    fitter = getattr(provider, "fit_points", None)
    if not callable(fitter):
        _fail(f"--fit-points needs per-stat totals and the platform's own fantasy points; "
              f"the {lc.provider} provider does not support it (try --league fantrax).")
    res = fitter()
    from .config import format_points

    source = getattr(provider, "scoring_source", "") or "points"
    cfg_mae = getattr(provider, "config_mae", float("nan"))
    use_mae = getattr(provider, "weights_mae", float("nan"))
    nan = lambda v: None if v is None or (isinstance(v, float) and math.isnan(v)) else v  # noqa: E731
    in_use = format_points(lc.scoring.weights, lc.scoring.goalie_weights)
    if as_json:
        _dump({"fit": {"weights": res.weights, "goalie_weights": res.goalie_weights, "groups": res.groups,
                       "mae": nan(res.mae), "n": res.n, "residual_max": nan(res.residual_max),
                       "points_line": res.points_line},
               "in_use": {"source": source, "points_line": "FANTRAX_POINTS=" + in_use, "mae": nan(use_mae)},
               "configured_mae": nan(cfg_mae), "warnings": lc.warnings})
        return
    if not res.n:
        _fail("No Fantrax lines with both per-stat totals and FPts were loaded; nothing to fit.")
    t = Table(title=f"Fitted point values ({res.n} Fantrax lines, least squares)")
    for c, j in (("Stat", "left"), ("Fitted", "right"), (f"In use ({source})", "right")):
        t.add_column(c, justify=j)  # type: ignore[arg-type]
    fitted = {**res.weights, **{f"goalie.{k}": v for k, v in res.goalie_weights.items()}}
    used = {**lc.scoring.weights, **{f"goalie.{k}": v for k, v in lc.scoring.goalie_weights.items()}}
    for k in sorted(set(fitted) | set(used), key=lambda k: (k.startswith("goalie."), -abs(fitted.get(k, 0.0)))):
        t.add_row(k, f"{fitted[k]:g}" if k in fitted else "-", f"{used[k]:g}" if k in used else "-")
    console.print(t)
    console.print("Paste into .env:")
    console.print(escape(res.points_line), soft_wrap=True, highlight=False)
    console.print(f"Fit error: mean abs {res.mae:.3f} FP/G, max {res.residual_max:.3f} FP/G over {res.n} lines")
    if cfg_mae == cfg_mae:  # not NaN
        console.print(f"Configured FANTRAX_POINTS: mean abs error {cfg_mae:.3f} FP/G"
                      + ("" if source == "FANTRAX_POINTS" else f" (not used: {source} win)"))
    if use_mae == use_mae and source != "FANTRAX_POINTS":
        console.print(f"In use ({source}): mean abs error {use_mae:.3f} FP/G")
        console.print(escape("FANTRAX_POINTS=" + in_use), soft_wrap=True, highlight=False)
    _footer(lc)


@app.command()
def settings(ctx: typer.Context,
             league: Optional[LeagueName] = typer.Option(None, "--league", "-l"),
             json_out: bool = typer.Option(False, "--json"),
             fit_points: bool = typer.Option(False, "--fit-points",
                                             help="Fit point values from the platform's own fantasy points "
                                                  "(Fantrax) and print a FANTRAX_POINTS line.")) -> None:
    """Show detected league settings: scoring, point values, roster shape, teams."""
    league_name, as_json = _opts(ctx, league, json_out)
    lc, provider = _load_context(league_name)
    if fit_points:
        _print_fit(lc, provider, as_json)
        return
    extra: list[str] = []
    describe = getattr(provider, "describe_settings", None)
    if callable(describe):
        try:
            extra = [str(x) for x in describe() or []]
        except Exception as e:
            lc.warnings.append(f"describe_settings failed: {e}")
    try:
        my = lc.my_team
    except LookupError:
        my = None
    if as_json:
        _dump({"provider": lc.provider, "league_id": lc.league_id, "season": lc.season, "name": lc.name,
               "scoring": lc.scoring.model_dump(), "roster_shape": lc.roster_shape,
               "matchup_period": lc.matchup_period, "my_team": my.name if my else None,
               "dynasty": lc.dynasty, "keeper_horizon_years": lc.keeper_horizon_years,
               "dynasty_mode": lc.dynasty_mode, "dynasty_mode_source": lc.dynasty_mode_source,
               "teams": [{"team_id": t.team_id, "name": t.name, "record": t.record, "mine": t.owner_is_me}
                         for t in lc.teams],
               "free_agents_loaded": len(lc.free_agents), "provider_settings": extra,
               "params_source": _params_source(), "warnings": lc.warnings})
        return
    console.print(f"[bold]{lc.name}[/]  ({lc.provider} league {lc.league_id}, season {lc.season})")
    console.print(f"Scoring: [cyan]{lc.scoring.kind}[/]   Matchup period: {lc.matchup_period}"
                  + (f"   Dynasty: yes ({lc.keeper_horizon_years}-year horizon, mode {lc.dynasty_mode})"
                     if lc.dynasty else ""))
    if lc.scoring.kind == "points":
        t = Table(title="Point values")
        t.add_column("Stat")
        t.add_column("Points", justify="right")
        for k, v in sorted(lc.scoring.weights.items(), key=lambda kv: -abs(kv[1])):
            t.add_row(k, f"{v:g}")
        for k, v in lc.scoring.goalie_weights.items():
            t.add_row(f"{k} (goalies)", f"{v:g}")
        console.print(t)
    else:
        console.print("Categories: " + ", ".join(lc.scoring.categories))
    shape = sorted(lc.roster_shape.items(), key=lambda kv: SLOT_ORDER.get(kv[0], 99))
    console.print("Roster: " + "  ".join(f"{s}x{n}" for s, n in shape))
    console.print(f"My team: [bold green]{my.name if my else '?'}[/]")
    tt = Table(title="Teams")
    for c in ("ID", "Team", "W-L-T"):
        tt.add_column(c)
    for team in lc.teams:
        rec = "-".join(map(str, team.record)) if team.record else ""
        name = f"[bold green]{team.name}[/]" if team.owner_is_me else team.name
        tt.add_row(team.team_id, name, rec)
    console.print(tt)
    console.print(f"Free agents loaded: {len(lc.free_agents)}")
    if extra:
        console.print("[bold]League rules detected by the provider[/]")
        for line in extra:
            console.print(f"  {line}")
    if not lc.dynasty and lc.provider == "fantrax":
        console.print(f"[dim]{escape(_mode_line(lc))} (used only when dynasty valuation is on)[/]")
    console.print(f"[dim]Valuation params: {escape(_params_source())} (`fm harness params`)[/]")
    _footer(lc)


@app.command()
def roster(ctx: typer.Context,
           all_teams: bool = typer.Option(False, "--all-teams", help="Show every team, not just mine."),
           deep: bool = typer.Option(False, "--deep", help="Also fetch NHL game logs for recent form."),
           league: Optional[LeagueName] = typer.Option(None, "--league", "-l"),
           mode: Optional[ModeName] = _mode_opt(),
           json_out: bool = typer.Option(False, "--json")) -> None:
    """Show roster(s) with per-game value, week projection, VORP (and dynasty value)."""
    league_name, as_json = _opts(ctx, league, json_out)
    lc, values, dyn = _load(league_name, deep, mode=_mode_val(mode))
    teams = lc.teams if all_teams else [lc.my_team]
    if as_json:
        _dump([{"team": t.name, "slots": [{"slot": s.slot, "starting": s.starting,
                                          "value": values[s.player.cid].model_dump(mode="json"),
                                          "dynasty": _dyn_json((dyn or {}).get(s.player.cid))}
                                         for s in t.slots if s.player]} for t in teams])
        return
    for team in teams:
        tbl = Table(title=f"{team.name}" + (" (me)" if team.owner_is_me else ""))
        cols = [("Slot", "left"), ("Player", "left"), ("Pos", "left"), ("Age", "right"), ("NHL", "left"),
                ("Status", "left"), ("GP", "right"), ("FPG szn", "right"), ("FPG wk", "right"), ("G/7d", "right"),
                ("Proj wk", "right"), ("VORP", "right")]
        if dyn is not None:
            cols.append(("Dynasty", "right"))
        for c, j in cols:
            tbl.add_column(c, justify=j)
        slots = sorted((s for s in team.slots if s.player), key=lambda s: SLOT_ORDER.get(s.slot, 99))
        for s in slots:
            p = s.player
            pv = values[p.cid]
            age = _age(p, lc, dyn)
            name = p.name + ("*" if _is_prospect(p, age) else "")
            row = [s.slot, name, _pos(p), "-" if age is None else str(int(age)), p.team or "-", _status(p),
                   str(p.gp("season")),
                   _fmt(pv.fpg_season), _fmt(pv.fpg_week),
                   "-" if pv.games_next7 is None else str(pv.games_next7), _fmt(pv.proj_week),
                   _fmt(pv.vorp, signed=True)]
            if dyn is not None:
                row.append(_fmt(_dyn_value(dyn.get(p.cid))))
            tbl.add_row(*row)
        console.print(tbl)
    repl = replacement_for(lc, values)
    console.print("Replacement FPG: " + "  ".join(f"{s} {v:.2f}" for s, v in repl.items()))
    console.print(f"[dim]* prospect: age <= {PROSPECT_MAX_AGE} and fewer than {PROSPECT_MAX_GP} NHL games[/]")
    _footer(lc)


@app.command()
def waivers(ctx: typer.Context,
            limit: int = typer.Option(10, "--limit", "-n", help="Max recommendations."),
            horizon: HorizonName = typer.Option(HorizonName.season, "--horizon",
                                                help="Value over the next week or rest of season."),
            deep: bool = typer.Option(False, "--deep", help="Also fetch NHL game logs for recent form."),
            league: Optional[LeagueName] = typer.Option(None, "--league", "-l"),
            mode: Optional[ModeName] = _mode_opt(),
            json_out: bool = typer.Option(False, "--json")) -> None:
    """Free-agent pickups that beat your weakest same-position player."""
    from .recommend.waivers import recommend_waivers

    league_name, as_json = _opts(ctx, league, json_out)
    lc, values, dyn = _load(league_name, deep, mode=_mode_val(mode))
    blocked: list[dict[str, Any]] = []
    recs = recommend_waivers(lc, values, limit=limit, horizon=horizon.value, dynasty_values=dyn,
                             debug=blocked)
    if as_json:
        _dump([r.model_dump(mode="json") for r in recs])
        return
    if not recs:
        msg = "No free agent beats your weakest same-position player by more than 0.3 FPG"
        if dyn:
            msg += " while also clearing the dynasty-value bar (x1.15, x1.5 for protected players)"
        console.print(msg + ".")
        _print_protected(blocked)
        _footer(lc)
        return
    tbl = Table(title=f"Waiver targets ({horizon.value} horizon)", show_lines=True)
    for c, j in (("#", "right"), ("Add", "left"), ("Drop", "left"), ("Gain", "right"),
                 ("Strength", "right"), ("Why", "left")):
        tbl.add_column(c, justify=j)
    h = horizon.value
    for i, r in enumerate(recs, 1):
        a = r.add[0]
        d = r.drop[0] if r.drop else None
        add = f"{a.name} ({_pos(a)}, {a.team or 'FA'})\n{values[a.cid].fpg_for(h):.2f} FPG"
        if d is None:
            ir_move = next((x.text for x in r.reasons if x.code == "IR_MOVE"), None)
            drop = ir_move or "(no drop: open roster spot)"
        else:
            drop = f"{d.name} ({_pos(d)})\n{values[d.cid].fpg_for(h):.2f} FPG"
        why = "\n".join(x.text for x in r.reasons if x.code not in ("FPG_ADD", "FPG_DROP"))
        tbl.add_row(_rank_cell(r, i), add, drop, _gain_cell(r), _strength_cell(r), why)
    console.print(tbl)
    _print_protected(blocked)
    _footer(lc)


def _print_protected(blocked: list[dict[str, Any]]) -> None:
    """One dim line per protected player that blocked at least one waiver drop."""
    by_drop: dict[str, list[dict[str, Any]]] = {}
    for b in blocked:
        by_drop.setdefault(b["drop"], []).append(b)
    for name, items in by_drop.items():
        best = max(items, key=lambda b: b["add_value"])
        console.print(f"[dim]Protected: {best['reason'].text} (blocked {len(items)} pickup"
                      f"{'s' if len(items) != 1 else ''})[/]")


@app.command()
def lineup(ctx: typer.Context,
           horizon: HorizonName = typer.Option(HorizonName.week, "--horizon",
                                               help="Optimize projected week points or season FPG."),
           deep: bool = typer.Option(False, "--deep", help="Also fetch NHL game logs for recent form."),
           league: Optional[LeagueName] = typer.Option(None, "--league", "-l"),
           json_out: bool = typer.Option(False, "--json")) -> None:
    """Optimal starting lineup and start/sit changes."""
    from .recommend.lineup import current_total, optimal_lineup, recommend_lineup, starting_slots

    league_name, as_json = _opts(ctx, league, json_out)
    lc, values, _ = _load(league_name, deep)
    h = horizon.value
    team = lc.my_team
    slots = starting_slots(lc.roster_shape)
    assign, total = optimal_lineup(team, values, lc.roster_shape, h)
    cur_total = current_total(team, values, h)
    recs = recommend_lineup(lc, values, h)
    where = {s.player.cid: s.slot for s in team.slots if s.player}
    by_cid = {p.cid: p for p in team.players}
    rows = []
    for i, slot in enumerate(slots):
        cid = assign.get(i)
        p = by_cid.get(cid) if cid else None
        pv = values.get(cid) if cid else None
        rows.append({"slot": slot, "cid": cid, "name": p.name if p else None,
                     "current_slot": where.get(cid) if cid else None,
                     "value": pv.lineup_value(h) if pv else None,
                     "games_next7": pv.games_next7 if pv else None})
    if as_json:
        _dump({"horizon": h, "optimal": rows, "optimal_total": total, "current_total": cur_total,
               "recommendations": [r.model_dump(mode="json") for r in recs], "meta": _meta(lc)})
        return
    unit = "proj pts" if h == "week" and any(v.proj_week is not None for v in values.values()) else "FPG"
    tbl = Table(title=f"Optimal lineup ({h}, {unit})")
    for c, j in (("Slot", "left"), ("Player", "left"), ("Pos", "left"), ("NHL", "left"), ("Status", "left"),
                 ("G/7d", "right"), ("Value", "right"), ("Now", "left")):
        tbl.add_column(c, justify=j)
    for r in rows:
        p = by_cid.get(r["cid"]) if r["cid"] else None
        if p is None:
            tbl.add_row(r["slot"], "[dim](empty)[/]", "", "", "", "", "", "")
            continue
        now = r["current_slot"] or "-"
        moved = now in ("BN", "IR")
        tbl.add_row(r["slot"], p.name, _pos(p), p.team or "-", _status(p),
                    "-" if r["games_next7"] is None else str(r["games_next7"]), _fmt(r["value"]),
                    f"[bold yellow]{now}[/]" if moved else now)
    console.print(tbl)
    console.print(f"Current lineup: {cur_total:.2f}   Optimal: {total:.2f}   Gain: {total - cur_total:+.2f}")
    if recs:
        console.print(_rec_table(recs, "Lineup changes"))
    else:
        console.print("[green]Your lineup is already optimal.[/]")
    _footer(lc)


@app.command()
def injuries(ctx: typer.Context,
             record: bool = typer.Option(True, "--record/--no-record",
                                         help="Save today's statuses as the baseline for the next run."),
             league: Optional[LeagueName] = typer.Option(None, "--league", "-l"),
             json_out: bool = typer.Option(False, "--json")) -> None:
    """Injury alerts for your roster: status changes, IR moves and IR activations."""
    from .recommend.injuries import StatusHistory, recommend_injuries

    league_name, as_json = _opts(ctx, league, json_out)
    lc, values, _ = _load(league_name, False)
    history = StatusHistory(get_settings().fm_data_dir)
    try:
        first_run = not history.last()
        recs = recommend_injuries(lc, values, history)
        written = history.record([p for t in lc.teams for p in t.players]) if record else 0
    finally:
        history.close()
    team = lc.my_team
    hurt = [(s.slot, s.player) for s in team.slots
            if s.player is not None and s.player.status not in ("healthy", "unknown")]
    if as_json:
        _dump({"injured": [{"slot": slot, "cid": p.cid, "name": p.name, "status": p.status,
                            "note": p.status_note} for slot, p in hurt],
               "recommendations": [r.model_dump(mode="json") for r in recs],
               "first_snapshot": first_run, "status_changes_recorded": written, "meta": _meta(lc)})
        return
    if hurt:
        tbl = Table(title=f"{team.name}: injured / unavailable")
        for c in ("Slot", "Player", "Pos", "NHL", "Status", "Note"):
            tbl.add_column(c)
        for slot, p in hurt:
            tbl.add_row(slot, p.name, _pos(p), p.team or "-", _status(p), p.status_note or "")
        console.print(tbl)
    else:
        console.print("[green]No injured players on your roster.[/]")
    if recs:
        console.print(_rec_table(recs, "Injury actions"))
    elif first_run:
        console.print("First snapshot saved; status changes will be reported from the next run.")
    _footer(lc)


@app.command()
def sync(ctx: typer.Context,
         review: bool = typer.Option(False, "--review", help="List player matches waiting for review."),
         confirm: Optional[list[str]] = typer.Option(None, "--confirm",
                                                     help="Pin a match, e.g. espn:123=8478402 (repeatable)."),
         refresh: bool = typer.Option(False, "--refresh", help="Clear the HTTP cache before loading."),
         deep: bool = typer.Option(False, "--deep", help="Also fetch NHL game logs."),
         league: Optional[LeagueName] = typer.Option(None, "--league", "-l"),
         json_out: bool = typer.Option(False, "--json")) -> None:
    """Refresh league + NHL data and manage the player id crosswalk."""
    from .matching.crosswalk import Crosswalk, parse_confirm

    league_name, as_json = _opts(ctx, league, json_out)
    settings_ = get_settings()
    out: dict[str, Any] = {}
    if confirm:
        xw = Crosswalk(settings_.fm_data_dir)
        try:
            done = []
            for spec in confirm:
                try:
                    src, sid, nid = parse_confirm(spec)
                except ValueError as e:
                    _fail(str(e))
                done.append(xw.confirm(src, sid, nid).model_dump())
        finally:
            xw.close()
        out["confirmed"] = done
        if not as_json:
            for d in done:
                console.print(f"Confirmed {d['source']}:{d['source_id']} ({d['name'] or '?'}) -> NHL {d['nhl_id']}")
    if review:
        xw = Crosswalk(settings_.fm_data_dir)
        try:
            rows = xw.pending(include_unmatched=True)
        finally:
            xw.close()
        out["pending"] = [r.model_dump() for r in rows]
        if not as_json:
            if not rows:
                console.print("[green]No player matches need review.[/]")
            else:
                tbl = Table(title="Player matches to review")
                for c in ("Player", "Team", "Source id", "Best NHL match", "NHL id", "Score", "State"):
                    tbl.add_column(c)
                for r in rows:
                    tbl.add_row(r.name, r.team or "-", f"{r.source}:{r.source_id}", r.candidate_name or "-",
                                str(r.nhl_id or "-"), f"{r.score:.0f}" if r.score else "-", r.confidence)
                console.print(tbl)
                console.print("[dim]Confirm with: fm sync --confirm SOURCE:ID=NHLID[/]")
    if refresh or not (review or confirm):
        cache = _cache()
        if refresh:
            cache.clear()
            if not as_json:
                console.print("HTTP cache cleared.")
        lc, values, _ = _load(league_name, deep, cache)
        players = lc.all_players()
        out["sync"] = {"players": len(players), "with_nhl_id": sum(1 for p in players if p.nhl_id is not None),
                       "valued": len(values), "meta": _meta(lc)}
        if not as_json:
            s = out["sync"]
            console.print(f"Synced {lc.name}: {s['players']} players, {s['with_nhl_id']} linked to NHL ids.")
            _footer(lc)
    if as_json:
        _dump(out)



# -- milestone 4/5 helpers ------------------------------------------------------

KIND_TITLE = {"injury": "Injuries", "lineup": "Lineup", "waiver": "Waivers", "trade": "Trades",
              "sell_high": "Sell high", "buy_low": "Buy low"}
LLM_HINT = ("LLM explanations are off: set OPENROUTER_API_KEY in .env (free models work: "
            "FM_LLM_MODEL=openrouter/free).")
WEB_HINT = "The web dashboard needs the optional web extras: pip install -e .[web]"


def _short_err(e: BaseException) -> str:
    msg = " ".join(str(e).split())
    return f"{type(e).__name__}: {msg[:200]}" if msg else type(e).__name__


def _safe(errors: list[str], label: str, fn: Callable[..., Any], *args: Any, default: Any = None,
          **kwargs: Any) -> Any:
    """Run one recommender/source; a failure becomes an entry in ``errors`` instead of a crash."""
    try:
        return fn(*args, **kwargs)
    except typer.Exit:
        raise
    except Exception as e:
        errors.append(f"{label} failed ({_short_err(e)})")
        return default


class _LogCollector(logging.Handler):
    """Collects WARNING+ records (advise() logs engine failures instead of raising)."""

    def __init__(self, sink: list[str], label: str):
        super().__init__(logging.WARNING)
        self.sink, self.label = sink, label

    def emit(self, record: logging.LogRecord) -> None:
        try:
            self.sink.append(f"{self.label}: {record.getMessage()}")
        except Exception:
            pass


def _collect_logs(logger_name: str, sink: list[str], label: str) -> "_LogScope":
    return _LogScope(logging.getLogger(logger_name), _LogCollector(sink, label))


class _LogScope:
    def __init__(self, logger: logging.Logger, handler: logging.Handler):
        self.logger, self.handler = logger, handler

    def __enter__(self) -> None:
        self.logger.addHandler(self.handler)

    def __exit__(self, *exc: Any) -> None:
        self.logger.removeHandler(self.handler)


def _json_out(payload: dict[str, Any], lc: LeagueContext | None, errors: list[str]) -> None:
    out = dict(payload)
    out["errors"] = list(errors)
    if lc is not None:
        out["meta"] = _meta(lc)
    _dump(out)


def _recs_json(recs: list[Recommendation] | None) -> list[dict[str, Any]]:
    return [r.model_dump(mode="json") for r in recs or []]


def _reason(r: Recommendation, code: str) -> Any:
    return next((x for x in r.reasons if x.code == code), None)


def _reason_val(r: Recommendation, code: str) -> float | None:
    x = _reason(r, code)
    return x.value if x is not None else None


def _trade_wants(lc: LeagueContext, provider: Any) -> dict[str, set[str]] | None:
    """team_id -> positions wanted, from Fantrax trade blocks (positions wanted plus the
    positions of specific players wanted). None when the provider has no trade blocks."""
    blocks = getattr(provider, "trade_blocks", None) or []
    by_cid = {p.cid: p for p in lc.all_players()}
    wants: dict[str, set[str]] = {}
    for b in blocks:
        tid = str(getattr(b, "team_id", "") or "")
        pos = {x for x in (getattr(b, "positions_wanted", None) or []) if x and x not in ("BN", "IR", "UTIL")}
        for cid in getattr(b, "players_wanted", None) or []:
            p = by_cid.get(cid)
            if p is not None:
                pos |= {x for x in p.positions if x != "F"}
        if tid and pos:
            wants[tid] = pos
    return wants or None


def _history() -> Any:
    from .recommend.injuries import StatusHistory

    return StatusHistory(get_settings().fm_data_dir)


def _run_advise(d: Loaded, history: Any, errors: list[str], limit: int | None = None
                ) -> list[Recommendation]:
    """advise() for lineup/waivers/flags/injuries plus recommend_trades called directly (so
    Fantrax trade-block `wants` reach it; advise() cannot forward that argument), merged
    and ordered exactly as advise() orders them."""
    from .recommend.advise import KIND_GROUP, PRIORITY, advise, normalize_scores
    from .recommend.trades import recommend_trades

    with _collect_logs("fantasy_manager.recommend.advise", errors, "advise"):
        recs = _safe(errors, "advise", advise, d.lc, d.values, dynasty_values=d.dyn, history=history,
                     include=("lineup", "waivers", "flags", "injuries"), default=[])
    trades = _safe(errors, "trades", recommend_trades, d.lc, d.values, dynasty_values=d.dyn,
                   wants=_trade_wants(d.lc, d.provider), default=[])
    merged = list(recs) + normalize_scores(trades)

    def prio(r: Recommendation) -> int:
        g = KIND_GROUP.get(r.kind, "flags")
        return PRIORITY.index(g) if g in PRIORITY else len(PRIORITY)

    merged.sort(key=lambda r: (-r.score, prio(r)))
    return merged[:limit] if limit else merged


def _llm_client() -> Any:
    from .llm.openrouter import LLMClient

    return LLMClient(get_settings())


def _fetch_news(cache: HttpCache, errors: list[str]) -> list[Any]:
    """All news sources through the HTTP cache (RSS TTL). A failing source becomes a warning.
    (fetch_all_news only logs failures, so the per-source fetchers are called here.)"""
    from .providers.enrich import make_fetch_text
    from .providers.news import fetch_espn_news, fetch_rotowire

    fetch = make_fetch_text(cache)
    items: list[Any] = []
    for label, fn in (("RotoWire news", fetch_rotowire), ("ESPN news", fetch_espn_news)):
        try:
            items.extend(fn(fetch))
        except Exception as e:
            reason = "offline and not cached" if isinstance(e, CacheMiss) else _short_err(e)
            errors.append(f"{label} unavailable ({reason})")
    epoch = datetime.min.replace(tzinfo=timezone.utc)
    items.sort(key=lambda n: n.published or epoch, reverse=True)
    return items


def _match_news(news: list[Any], players: list[Any], errors: list[str]) -> dict[str, list[Any]]:
    from .report.news_match import match_news_to_players

    return _safe(errors, "news matching", match_news_to_players, news, players, default={}) or {}


def _explain(recs: list[Recommendation], d: Loaded, errors: list[str], news: list[Any] | None = None,
             notes: list[str] | None = None) -> list[Any]:
    """Attach LLM narratives when a key is configured; otherwise add a one-line hint.
    Returns the news list used (fetched here when not supplied)."""
    from .llm.openrouter import narrate

    client = _llm_client()
    if not client.available:
        (notes if notes is not None else errors).append(LLM_HINT)
        return news or []
    if news is None:
        news = _fetch_news(d.cache, errors)
    players = [p for r in recs for p in (*r.add, *r.drop)]
    by_cid = _match_news(news, players, errors)
    _safe(errors, "LLM narration", narrate, recs, d.lc, by_cid, client)
    if recs and not any(r.narrative for r in recs):
        errors.append("LLM narration produced no usable explanations (see logs); showing heuristics only")
    return news


def _age_text(when: datetime | None, now: datetime | None = None) -> str:
    if when is None:
        return "undated"
    now = now or datetime.now(timezone.utc)
    secs = max(0.0, (now - when).total_seconds())
    if secs < 3600:
        return f"{secs / 60:.0f}m ago"
    if secs < 48 * 3600:
        return f"{secs / 3600:.0f}h ago"
    return f"{secs / 86400:.0f}d ago"


# -- milestone 4 commands ---------------------------------------------------------

@app.command()
def trades(ctx: typer.Context,
           limit: int = typer.Option(10, "--limit", "-n", help="Max proposals overall."),
           per_team: int = typer.Option(3, "--per-team", help="Max proposals per counterparty."),
           deep: bool = typer.Option(False, "--deep", help="Also fetch NHL game logs for recent form."),
           league: Optional[LeagueName] = typer.Option(None, "--league", "-l"),
           mode: Optional[ModeName] = _mode_opt(),
           json_out: bool = typer.Option(False, "--json")) -> None:
    """Trade proposals: 1-for-1 and 2-for-1 deals that improve your lineup and are fair."""
    from .recommend.trades import recommend_trades

    league_name, as_json = _opts(ctx, league, json_out)
    d = _load_all(league_name, deep, mode=_mode_val(mode))
    lc, errors = d.lc, d.errors
    wants = _trade_wants(lc, d.provider)
    future: list[Recommendation] = []
    recs = _safe(errors, "trades", recommend_trades, lc, d.values, dynasty_values=d.dyn,
                 max_per_team=per_team, limit=limit, wants=wants, default=[],
                 future_only=future if d.dyn and lc.dynasty_mode == "contend" else None)
    if as_json:
        _json_out({"trades": _recs_json(recs), "future_only": _recs_json(future[:5]),
                   "trade_block_wants": {k: sorted(v) for k, v in (wants or {}).items()}},
                  lc, errors)
        return
    if not recs:
        console.print("No trade passes the fairness band with a lineup gain above +0.5 for you.")
        _print_future(future)
        _footer(lc, errors)
        return
    units = "dynasty value" if d.dyn else "VORP"
    tbl = Table(title=f"Trade proposals (Me/Them = starting-lineup FPG change; fairness on {units})",
                show_lines=True)
    for c, j in (("#", "right"), ("With", "left"), ("Give", "left"), ("Get", "left"), ("Me", "right"),
                 ("Them", "right"), ("Fair", "right"), ("Strength", "right"), ("Why", "left")):
        tbl.add_column(c, justify=j)
    for i, r in enumerate(recs, 1):
        gap = _reason_val(r, "FAIR_PCT")
        why = [x.text for x in r.reasons if x.code in ("DYNASTY_DELTA", "THEIR_NEED", "ROSTER_DROP", "FA_FILL",
                                                        "ROTO_BALANCE")]
        tbl.add_row(_rank_cell(r, i), r.counterparty or "-",
                    "\n".join(f"{p.name} ({_pos(p)})" for p in r.drop),
                    "\n".join(f"{p.name} ({_pos(p)})" for p in r.add),
                    _fmt(_reason_val(r, "DELTA_ME"), signed=True), _fmt(_reason_val(r, "DELTA_THEM"), signed=True),
                    "-" if gap is None else f"{gap:.0%} gap", _strength_cell(r), "\n".join(why) or "-")
    console.print(tbl)
    if wants:
        console.print("[dim]Trade-block wants: " + "; ".join(
            f"{next((t.name for t in lc.teams if t.team_id == k), k)}: {', '.join(sorted(v))}"
            for k, v in wants.items()) + "[/]")
    _print_future(future)
    _footer(lc, errors)


def _print_future(future: list[Recommendation], n: int = 3) -> None:
    """Contend mode: trades that add dynasty value but weaken this season's lineup."""
    if not future:
        return
    console.print("[bold]Future-only[/] (contend mode: not recommended, they cost this season's lineup)")
    for r in future[:n]:
        cost = next((x.text for x in r.reasons if x.code == "WIN_NOW_COST"), "")
        dyn = next((x.text for x in r.reasons if x.code == "DYNASTY_DELTA"), "")
        console.print(f"[dim]  {r.title}: {cost}; {dyn.split(';')[0]}[/]")


@app.command()
def flags(ctx: typer.Context,
          limit: int = typer.Option(10, "--limit", "-n", help="Max flags per table."),
          deep: bool = typer.Option(False, "--deep", help="Also fetch NHL game logs (needed for L15 form)."),
          league: Optional[LeagueName] = typer.Option(None, "--league", "-l"),
          json_out: bool = typer.Option(False, "--json")) -> None:
    """Sell-high (your hot, lucky players) and buy-low (cold players with intact shot rates)."""
    from .recommend.flags import recommend_flags

    league_name, as_json = _opts(ctx, league, json_out)
    d = _load_all(league_name, deep)
    lc, errors = d.lc, d.errors
    recs = _safe(errors, "flags", recommend_flags, lc, d.values, default=[]) or []
    sell = [r for r in recs if r.kind == "sell_high"][:limit]
    buy = [r for r in recs if r.kind == "buy_low"][:limit]
    if as_json:
        _json_out({"sell_high": _recs_json(sell), "buy_low": _recs_json(buy)}, lc, errors)
        return
    for title, rows, who in (("Sell high (your players)", sell, "drop"), ("Buy low (other teams)", buy, "add")):
        if not rows:
            console.print(f"[dim]{title}: none flagged.[/]")
            continue
        tbl = Table(title=title, show_lines=True)
        for c, j in (("#", "right"), ("Player", "left"), ("Team", "left"), ("Form", "right"),
                     ("Strength", "right"), ("Why", "left")):
            tbl.add_column(c, justify=j)
        for i, r in enumerate(rows, 1):
            p = (r.drop if who == "drop" else r.add)[0]
            form = _reason_val(r, "FORM_RATIO")
            tbl.add_row(str(i), f"{p.name} ({_pos(p)}, {p.team or '-'})", r.counterparty or "me",
                        "-" if form is None else f"{form:.2f}x", _strength_cell(r),
                        "\n".join(x.text for x in r.reasons if x.code != "FORM_RATIO"))
        console.print(tbl)
    if not deep and not recs:
        console.print("[dim]Flags need last-15-day splits; ESPN provides them, other leagues need --deep.[/]")
    _footer(lc, errors)


@app.command("advise")
def advise_cmd(ctx: typer.Context,
               limit: int = typer.Option(15, "--limit", "-n", help="Max recommendations overall."),
               explain: bool = typer.Option(False, "--explain", help="Add short LLM explanations (needs "
                                                                      "OPENROUTER_API_KEY)."),
               deep: bool = typer.Option(False, "--deep", help="Also fetch NHL game logs for recent form."),
               league: Optional[LeagueName] = typer.Option(None, "--league", "-l"),
               mode: Optional[ModeName] = _mode_opt(),
               json_out: bool = typer.Option(False, "--json")) -> None:
    """Everything at once: injuries, lineup, waivers, trades and flags, ranked."""
    from .recommend.advise import group_by_kind

    league_name, as_json = _opts(ctx, league, json_out)
    d = _load_all(league_name, deep, mode=_mode_val(mode))
    lc, errors = d.lc, d.errors
    notes: list[str] = []
    history = _history()
    try:
        recs = _run_advise(d, history, errors, limit)
    finally:
        history.close()
    if explain:
        _explain(recs, d, errors, notes=notes)
    if as_json:
        _json_out({"recommendations": _recs_json(recs), "notes": notes}, lc, errors)
        return
    if not recs:
        console.print("[green]Nothing to do right now.[/]")
    for kind, items in group_by_kind(recs).items():
        console.print(_rec_table(items, KIND_TITLE.get(kind, kind)))
    if recs:
        console.print("[dim]Strength is absolute (0-10 from the predicted gain on fixed per-kind scales); "
                      "# is the rank within the kind.[/]")
    for n in notes:
        console.print(f"[dim]{n}[/]")
    _footer(lc, errors)


# -- milestone 5 commands ---------------------------------------------------------

@app.command()
def news(ctx: typer.Context,
         mine: bool = typer.Option(True, "--mine/--all",
                                   help="--mine: your roster and free agents; --all: every player in the league."),
         limit: int = typer.Option(30, "--limit", "-n", help="Max news items shown."),
         league: Optional[LeagueName] = typer.Option(None, "--league", "-l"),
         json_out: bool = typer.Option(False, "--json")) -> None:
    """Player news (RotoWire, ESPN) matched to your roster and free agents."""
    league_name, as_json = _opts(ctx, league, json_out)
    d = _load_all(league_name, False)
    lc, errors = d.lc, d.errors
    owner: dict[str, str] = {}
    for p in lc.free_agents:
        owner.setdefault(p.cid, "FA")
    for t in lc.teams:
        if mine and not t.owner_is_me:
            continue
        for p in t.players:
            owner[p.cid] = "me" if t.owner_is_me else t.name
    players = [p for p in lc.all_players() if p.cid in owner]
    items = _fetch_news(d.cache, errors)
    by_cid = _match_news(items, players, errors)
    by_player = {p.cid: p for p in players}
    epoch = datetime.min.replace(tzinfo=timezone.utc)
    # players ordered by their newest item; mine first on ties
    order = sorted(by_cid, key=lambda c: (by_cid[c][0].published or epoch, owner.get(c) == "me"), reverse=True)
    shown, groups = 0, []
    for cid in order:
        if shown >= limit:
            break
        take = by_cid[cid][:max(0, limit - shown)]
        shown += len(take)
        groups.append((by_player[cid], take))
    if as_json:
        _json_out({"items_fetched": len(items),
                   "players": [{"cid": p.cid, "name": p.name, "owner": owner.get(p.cid),
                                "items": [n.model_dump(mode="json") for n in its]} for p, its in groups]},
                  lc, errors)
        return
    if not groups:
        what = "your roster or free agents" if mine else "league players"
        console.print(f"No news for {what}" + (" (no news could be fetched)." if not items else "."))
    for p, its in groups:
        who = {"me": "[bold green]my team[/]", "FA": "free agent"}.get(owner.get(p.cid, ""), escape(owner.get(p.cid, "")))
        console.print(f"[bold]{escape(p.name)}[/] ({_pos(p)}, {p.team or '-'}) - {who} {_status(p)}")
        for n in its:
            tags = f" [cyan]\\[{', '.join(n.tags)}][/]" if n.tags else ""
            console.print(f"  [dim]{_age_text(n.published)} {n.source}[/]{tags} {escape(n.headline)}")
            if n.blurb:
                blurb = n.blurb if len(n.blurb) <= 240 else n.blurb[:237].rstrip() + "..."
                console.print(f"    [dim]{escape(blurb)}[/]")
    _footer(lc, errors)


@app.command("ask")
def ask_cmd(ctx: typer.Context,
            question: str = typer.Argument(..., help='Your question, e.g. "should I trade X for Y?"'),
            league: Optional[LeagueName] = typer.Option(None, "--league", "-l"),
            json_out: bool = typer.Option(False, "--json")) -> None:
    """Ask the LLM a question about your league (needs OPENROUTER_API_KEY)."""
    from .llm.openrouter import LLMError, ask

    league_name, as_json = _opts(ctx, league, json_out)
    client = _llm_client()
    if not client.available:
        _fail("`fm ask` needs an LLM: set OPENROUTER_API_KEY in .env (get a key at openrouter.ai; "
              "FM_LLM_MODEL=openrouter/free works without credits).")
    d = _load_all(league_name, False)
    lc, errors = d.lc, d.errors
    history = _history()
    try:
        recs = _run_advise(d, history, errors, 15)
    finally:
        history.close()
    items = _fetch_news(d.cache, errors)
    try:
        answer = ask(question, lc, d.values, recs, items, client)
    except LLMError as e:
        _fail(str(e))
    if as_json:
        _json_out({"question": question, "answer": answer, "model": getattr(client, "model", None)}, lc, errors)
        return
    console.print(escape(answer))
    console.print(f"[dim]model: {getattr(client, 'model', '?')}[/]")
    _footer(lc, errors)


@app.command()
def report(ctx: typer.Context,
           out: Optional[Path] = typer.Option(None, "--out", "-o",
                                              help="Output folder (default: <FM_DATA_DIR>/reports)."),
           notify_: bool = typer.Option(False, "--notify", help="Post the summary to Discord/Slack webhooks."),
           explain: bool = typer.Option(False, "--explain", help="Add short LLM explanations (needs "
                                                                  "OPENROUTER_API_KEY)."),
           record: bool = typer.Option(True, "--record/--no-record",
                                       help="Save today's injury statuses as the baseline for the next run."),
           limit: int = typer.Option(25, "--limit", "-n", help="Max recommendations in the digest."),
           deep: bool = typer.Option(False, "--deep", help="Also fetch NHL game logs for recent form."),
           league: Optional[LeagueName] = typer.Option(None, "--league", "-l"),
           mode: Optional[ModeName] = _mode_opt(),
           json_out: bool = typer.Option(False, "--json")) -> None:
    """Write the daily digest (markdown + HTML) and optionally post a summary to webhooks."""
    from .report.digest import build_digest, model_headline, model_health, write_digest
    from .report.notify import any_failed, notify_all

    league_name, as_json = _opts(ctx, league, json_out)
    settings_ = get_settings()
    d = _load_all(league_name, deep, mode=_mode_val(mode))
    lc, errors = d.lc, d.errors
    history = _history()
    try:
        recs = _run_advise(d, history, errors, limit)
        if record:
            _safe(errors, "injury snapshot", history.record, [p for t in lc.teams for p in t.players])
    finally:
        history.close()
    items = _fetch_news(d.cache, errors)
    notes: list[str] = []
    if explain:
        _explain(recs, d, errors, news=items, notes=notes)
    by_cid = _match_news(items, lc.all_players(), errors)
    digest = build_digest(lc, d.values, recs, by_cid,
                          model_headline=model_headline(settings_.fm_data_dir, lc.provider),
                          model_health=model_health(settings_.fm_data_dir, lc.provider))
    out_dir = out or (Path(settings_.fm_data_dir) / "reports")
    try:
        md_path, html_path = write_digest(digest, out_dir)
    except OSError as e:
        _fail(f"could not write the report to {out_dir}: {e}")
    results: list[str] = []
    if notify_:
        results = notify_all(settings_, digest.summary)
    failed = notify_ and (any_failed(results) or not (settings_.discord_webhook_url or settings_.slack_webhook_url))
    if as_json:
        _json_out({"markdown": str(md_path), "html": str(html_path), "summary": digest.summary,
                   "recommendations": len(recs), "notify": results, "notes": notes}, lc, errors)
    else:
        console.print(f"Wrote {md_path}")
        console.print(f"Wrote {html_path}")
        console.print(f"{len(recs)} recommendation(s), {sum(len(v) for v in by_cid.values())} matched news item(s).")
        for r in results:
            style = "red" if ": error" in r else ("yellow" if "not configured" in r else "green")
            console.print(f"[{style}]{escape(r)}[/]")
        for n in notes:
            console.print(f"[dim]{n}[/]")
        _footer(lc, errors)
    if failed:
        raise typer.Exit(1)


@app.command()
def notify(ctx: typer.Context,
           text: str = typer.Argument(..., help="Message to send (useful for testing webhooks)."),
           json_out: bool = typer.Option(False, "--json")) -> None:
    """Send a message to the configured Discord/Slack webhooks."""
    from .report.notify import any_failed, notify_all

    _, as_json = _opts(ctx, None, json_out)
    s = get_settings()
    results = notify_all(s, text)
    configured = bool(s.discord_webhook_url or s.slack_webhook_url)
    if as_json:
        _dump({"results": results, "configured": configured})
    else:
        for r in results:
            style = "red" if ": error" in r else ("green" if configured else "yellow")
            console.print(f"[{style}]{escape(r)}[/]")
    if not configured or any_failed(results):
        raise typer.Exit(1)


@app.command("mode")
def mode_cmd(ctx: typer.Context,
             mode: Optional[ModeName] = typer.Argument(None, help="contend, balanced or rebuild. Omit to show "
                                                                  "the current mode."),
             clear: bool = typer.Option(False, "--clear", help="Forget the saved mode (fall back to FANTRAX_MODE)."),
             json_out: bool = typer.Option(False, "--json")) -> None:
    """Show or set the dynasty mode used by every command and the dashboard (saved in prefs.json)."""
    from .prefs import DYNASTY_MODE_KEY, dynasty_mode_info, mode_source_label, prefs_path, set_pref

    _, as_json = _opts(ctx, None, json_out)
    if mode is not None and clear:
        _fail("give a mode or --clear, not both")
    settings_ = get_settings()
    path = prefs_path(settings_.fm_data_dir)
    try:
        if mode is not None:
            set_pref(DYNASTY_MODE_KEY, mode.value, data_dir=settings_.fm_data_dir)
        elif clear:
            set_pref(DYNASTY_MODE_KEY, None, data_dir=settings_.fm_data_dir)
    except OSError as e:
        _fail(f"could not save {path}: {e}")
    current, source = dynasty_mode_info(settings_)
    if as_json:
        _dump({"mode": current, "source": source, "source_label": mode_source_label(source),
               "prefs_file": str(path), "changed": mode is not None or clear})
        return
    if mode is not None:
        console.print(f"Dynasty mode set to [bold]{current}[/] (saved to {escape(str(path))}; "
                      "a running dashboard recalculates on its next page load).")
    elif clear:
        console.print(f"Saved dynasty mode cleared; now [bold]{current}[/] (from {mode_source_label(source)}).")
    else:
        console.print(f"Dynasty mode: [bold]{current}[/] (from {mode_source_label(source)})")
        console.print("[dim]Set it with `fm mode contend|balanced|rebuild` or the dashboard's Mode toggle "
                      "(saved, wins over FANTRAX_MODE), or --mode on a single command.[/]")


auth_app = typer.Typer(help="Log in to a league site and manage the saved session.", no_args_is_help=True)
app.add_typer(auth_app, name="auth")


def _ts(v: float | None) -> str | None:
    return datetime.fromtimestamp(v, timezone.utc).astimezone().isoformat(timespec="seconds") if v else None


@auth_app.command("fantrax")
def auth_fantrax(ctx: typer.Context,
                 login: bool = typer.Option(False, "--login", help="Log in now with FANTRAX_USERNAME / "
                                            "FANTRAX_PASSWORD and save the session."),
                 status: bool = typer.Option(False, "--status", help="Show the saved session and which "
                                             "credentials are configured (present/absent only)."),
                 logout: bool = typer.Option(False, "--logout", help="Delete the saved session file."),
                 ping: bool = typer.Option(False, "--ping", help="Make one cheap authenticated request "
                                           "(keeps the session in use; re-logs in if needed)."),
                 json_out: bool = typer.Option(False, "--json")) -> None:
    """Fantrax login: --login, --status (default), --logout, --ping. Never prints cookies or passwords."""
    from .providers.fantrax import FantraxProvider
    from .providers.fantrax_auth import FantraxAuth, fmt_age

    _, as_json = _opts(ctx, None, json_out)
    if sum((login, logout, ping)) > 1:
        _fail("give only one of --login, --logout, --ping (with --status if you like)")
    settings_ = get_settings()
    auth = FantraxAuth(settings_)
    out: dict[str, Any] = {}
    if logout:
        out["logged_out"] = auth.logout()
        if not as_json:
            console.print("Saved Fantrax session deleted." if out["logged_out"] else "No saved Fantrax session.")
    if login:
        try:
            auth.refresh()
        except ProviderError as e:
            _fail(str(e))
        out["login"] = "ok"
        if not as_json:
            console.print(f"Logged in to Fantrax; session saved to {escape(str(auth.path))}.")
    if ping:
        if settings_.fm_offline:
            _fail("--ping needs the network; unset FM_OFFLINE")
        provider = FantraxProvider(settings_, auth=auth)
        try:
            out["ping"] = provider.ping()
        except ProviderError as e:
            _fail(str(e))
        if not as_json:
            p = out["ping"]
            name = f" ({escape(str(p['league_name']))})" if p.get("league_name") else ""
            console.print(f"Fantrax session OK for league {escape(str(p['league_id']))}{name}; "
                          f"session from {p['session_source']}.")
            for w in provider.warnings:
                console.print(f"[dim yellow]! {escape(w)}[/]")
    if status or not (login or logout or ping):
        st = auth.status()
        out["status"] = {**st, "last_login_attempt": _ts(st["last_login_attempt"]),
                         "last_ping_at": _ts(st["last_ping_at"])}
        if not as_json:
            if st["saved_session"]:
                state = "known expired" if st["saved_session_expired"] else "in use first"
                console.print(f"Saved session: yes, {fmt_age(st['saved_session_age_seconds'])} old, from "
                              f"{st['saved_session_source'] or '?'} ({state}); {escape(st['session_file'])}")
            else:
                console.print(f"Saved session: none ({escape(st['session_file'])})")
            creds = st["credentials"]
            console.print("Configured: " + ", ".join(f"{k} {'present' if v else 'absent'}" for k, v in creds.items()))
            if st["last_login_attempt"]:
                res = {True: "succeeded", False: "failed"}.get(st["last_login_ok"], "unknown")
                err_ = f" ({escape(str(st['last_login_error']))})" if st["last_login_ok"] is False else ""
                console.print(f"Last login: {res}{err_}, {fmt_age(auth.clock() - st['last_login_attempt'])} ago")
            else:
                console.print("Last login: never")
            if st["last_ping_at"]:
                console.print(f"Last ping: {'ok' if st['last_ping_ok'] else 'failed'}, "
                              f"{fmt_age(auth.clock() - st['last_ping_at'])} ago")
            if st["login_allowed_in_seconds"] > 0:
                console.print(f"[dim]Next login attempt allowed in {fmt_age(st['login_allowed_in_seconds'])}.[/]")
            if not (creds["FANTRAX_USERNAME"] and creds["FANTRAX_PASSWORD"]):
                console.print("[dim]Tip: set FANTRAX_USERNAME and FANTRAX_PASSWORD in .env so fm logs in and "
                              "refreshes the session itself.[/]")
    if as_json:
        _dump(out)


@app.command()
def web(ctx: typer.Context,
        host: str = typer.Option("127.0.0.1", "--host",
                                 help="Interface to bind (127.0.0.1 = this PC only; 0.0.0.0 = every interface, "
                                      "needs FM_WEB_PASSWORD or FM_WEB_ALLOW_INSECURE=1)."),
        port: int = typer.Option(8765, "--port", help="Port to listen on."),
        league: Optional[LeagueName] = typer.Option(None, "--league", "-l")) -> None:
    """Start the mobile-friendly web dashboard (needs the optional web extras).

    With FM_WEB_PASSWORD set every page needs a login. Without it the dashboard answers only on this
    machine, and a non-loopback --host is refused unless FM_WEB_ALLOW_INSECURE=1."""
    league_name, _ = _opts(ctx, league, False)
    try:
        import fastapi  # noqa: F401
        import uvicorn  # noqa: F401
    except ImportError:
        _fail(WEB_HINT)
    try:
        from .web.app import run
    except ImportError as e:
        missing = (getattr(e, "name", None) or "").split(".")[0]
        if missing in ("fastapi", "uvicorn", "jinja2", "starlette", "multipart"):
            _fail(WEB_HINT)
        _fail(f"the web dashboard is not available ({_short_err(e)})")
    try:  # run() prints the URL once the access checks pass
        run(host, port, league_name)
    except ValueError as e:  # auth.InsecureBindError: non-loopback host without FM_WEB_PASSWORD
        _fail(str(e))


if __name__ == "__main__":
    app()
