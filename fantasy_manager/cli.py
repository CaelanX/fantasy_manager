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


MONEYPUCK_CREDIT = "Expected goals: MoneyPuck.com"
DFO_CREDIT = "Lines, power-play units and starting goalies: Daily Faceoff (dailyfaceoff.com)"


def credit_lines(lc: LeagueContext) -> list[str]:
    """Source credits owed when the data was used: MoneyPuck (xG) and Daily Faceoff (lines /
    starting goalies), unless a source note already carries them."""
    players = lc.all_players()
    notes = " ".join(lc.source_notes)
    out = []
    if any(p.ixg_per_game is not None for p in players) and "MoneyPuck" not in notes:
        out.append(MONEYPUCK_CREDIT)
    if any(p.line or p.pp_unit or p.confirmed_start is not None for p in players) and "Daily Faceoff" not in notes:
        out.append(DFO_CREDIT)
    return out


def _footer(lc: LeagueContext, errors: list[str] | None = None) -> None:
    if lc.dynasty:
        console.print(f"[dim]{escape(_mode_line(lc))}; {lc.keeper_horizon_years}-year horizon "
                      "(change with `fm mode contend|balanced|rebuild` or the dashboard toggle)[/]")
    for n in [*lc.source_notes, *credit_lines(lc)]:
        console.print(f"[dim]{escape(n)}[/]")
    for w in [*lc.warnings, *(errors or [])]:
        console.print(f"[dim yellow]! {w}[/]")


def _params_source() -> str:
    try:
        from .valuation.params import source

        return source()
    except Exception as e:  # noqa: BLE001
        return f"unavailable ({type(e).__name__})"


def _vlabel(lc: Any) -> str:
    """Column label for per-game value: FPG in points leagues, Val/G (z-score sum) in categories/roto."""
    kind = getattr(getattr(lc, "scoring", None), "kind", "points")
    return "FPG" if kind == "points" else "Val/G"


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


def _moves_line(lc: LeagueContext) -> str:
    """"Moves: 2 per matchup period (1 used, 1 left, resets Mon Oct 5)" / "Moves: unlimited"."""
    from .recommend.base import moves_text

    line = f"Moves: {moves_text(lc)}"
    if lc.faab_remaining is not None:
        line += f"; FAAB left ${lc.faab_remaining:g}"
    return line


def _moves_json(lc: LeagueContext) -> dict[str, Any]:
    from .recommend.base import moves_left

    return {"limit_per_period": lc.moves_limit_per_period, "period": lc.moves_period_label,
            "used_this_period": lc.moves_used_this_period, "left": moves_left(lc),
            "limit_season": lc.moves_limit_season, "used_season": lc.moves_used_season,
            "period_start": lc.period_start.isoformat() if lc.period_start else None,
            "period_end": lc.period_end.isoformat() if lc.period_end else None,
            "recent_adds": {k: v.isoformat() for k, v in lc.recent_adds.items()},
            "faab_remaining": lc.faab_remaining}


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
               "position_limits": lc.position_limits, "max_roster_size": lc.max_roster_size,
               "matchup_period": lc.matchup_period, "my_team": my.name if my else None,
               "dynasty": lc.dynasty, "keeper_horizon_years": lc.keeper_horizon_years,
               "dynasty_mode": lc.dynasty_mode, "dynasty_mode_source": lc.dynasty_mode_source,
               "teams": [{"team_id": t.team_id, "name": t.name, "record": t.record, "mine": t.owner_is_me}
                         for t in lc.teams],
               "free_agents_loaded": len(lc.free_agents), "provider_settings": extra,
               "moves": _moves_json(lc),
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
    from .recommend.base import position_limits_text
    console.print("Roster maximums: " + (position_limits_text(lc) or "none reported by the provider"))
    console.print(escape(_moves_line(lc)))
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
                ("Status", "left"), ("GP", "right"), (f"{_vlabel(lc)} szn", "right"), (f"{_vlabel(lc)} wk", "right"), ("G/7d", "right"),
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
    console.print(f"[dim]{escape(_moves_line(lc))}[/]")
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
        from .recommend.base import move_scarcity_threshold, moves_left, no_moves_text

        if moves_left(lc) == 0:
            console.print(escape(no_moves_text(lc)) + ".")
        else:
            thr = move_scarcity_threshold(lc) or 0.3
            msg = f"No free agent beats your weakest same-position player by more than {thr:.1f} FPG"
            if dyn:
                msg += " while also clearing the dynasty-value bar (x1.15, x1.5 for protected players)"
            console.print(msg + ".")
        _print_protected(blocked)
        console.print(f"[dim]{escape(_moves_line(lc))}[/]")
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
    console.print(f"[dim]{escape(_moves_line(lc))}[/]")
    _footer(lc)


def _print_protected(blocked: list[dict[str, Any]]) -> None:
    """One dim line per protected player that blocked at least one waiver drop."""
    by_drop: dict[str, list[dict[str, Any]]] = {}
    capped = [b for b in blocked if b["reason"].code == "POSITION_CAP"]
    churn: dict[str, list[dict[str, Any]]] = {}
    for b in blocked:
        if b["reason"].code == "CHURN_GUARD":
            churn.setdefault(b["drop"], []).append(b)
        elif b["reason"].code != "POSITION_CAP":
            by_drop.setdefault(b["drop"], []).append(b)
    for name, items in churn.items():
        best = max(items, key=lambda b: b["add_value"] or 0.0)
        console.print(f"[dim]Churn guard: {escape(best['reason'].text)} (blocked {len(items)} pickup"
                      f"{'s' if len(items) != 1 else ''})[/]")
    for name, items in by_drop.items():
        best = max(items, key=lambda b: b["add_value"])
        console.print(f"[dim]Protected: {best['reason'].text} (blocked {len(items)} pickup"
                      f"{'s' if len(items) != 1 else ''})[/]")
    for b in capped[:3]:
        console.print(f"[dim]Position limit: {escape(b['reason'].text)}[/]")


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
              "sell_high": "Sell high", "buy_low": "Buy low", "alert": "Alerts"}
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
    """advise() for lineup/waivers/flags/injuries/alerts plus recommend_trades called directly (so
    Fantrax trade-block `wants` reach it; advise() cannot forward that argument), merged
    and ordered exactly as advise() orders them."""
    from .recommend.advise import KIND_GROUP, PRIORITY, advise, normalize_scores
    from .recommend.trades import recommend_trades

    with _collect_logs("fantasy_manager.recommend.advise", errors, "advise"):
        recs = _safe(errors, "advise", advise, d.lc, d.values, dynasty_values=d.dyn, history=history,
                     include=("lineup", "waivers", "flags", "injuries", "alerts"), default=[])
    trades = _safe(errors, "trades", recommend_trades, d.lc, d.values, dynasty_values=d.dyn,
                   wants=_trade_wants(d.lc, d.provider), offered=_trade_offered(d.provider), default=[])
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
           exploit_limit: int = typer.Option(8, "--exploits", help="Max exploit trades (teams under roster "
                                                                     "pressure); 0 = skip."),
           json_out: bool = typer.Option(False, "--json")) -> None:
    """Trade proposals ranked by expected value: your lineup gain x the chance they accept
    (judged by market value: ADP / % rostered). Plus the sweet spot: deals the market calls fair
    that our model says you win, and exploits: deals that relieve another team's roster pressure."""
    from .recommend.trades import exploit_opportunities, recommend_trades

    league_name, as_json = _opts(ctx, league, json_out)
    d = _load_all(league_name, deep, mode=_mode_val(mode))
    lc, errors = d.lc, d.errors
    wants = _trade_wants(lc, d.provider)
    offered = _trade_offered(d.provider)
    future: list[Recommendation] = []
    sweet: list[Recommendation] = []
    recs = _safe(errors, "trades", recommend_trades, lc, d.values, dynasty_values=d.dyn,
                 max_per_team=per_team, limit=limit, wants=wants, offered=offered,
                 default=[], sweet_spot=sweet,
                 future_only=future if d.dyn and lc.dynasty_mode == "contend" else None)
    exploits = (_safe(errors, "exploit trades", exploit_opportunities, lc, d.values, dynasty_values=d.dyn,
                      limit=exploit_limit, wants=wants, offered=offered, default=[]) or []) if exploit_limit > 0 else []
    if as_json:
        _json_out({"trades": _recs_json(recs), "sweet_spot": _recs_json(sweet), "future_only": _recs_json(future[:5]),
                   "exploits": _recs_json(exploits),
                   "trade_block_wants": {k: sorted(v) for k, v in (wants or {}).items()}},
                  lc, errors)
        return
    if not recs:
        console.print("No trade gains you more than +0.3 with at least a 25% chance of being accepted.")
        _print_exploits(exploits)
        _print_future(future)
        _footer(lc, errors)
        return
    console.print(_trade_table("Trade proposals, ranked by expected value (EV = your gain x acceptance chance)",
                               recs))
    console.print("[dim]You gain = your lineup's points per game (/g), per week (/wk, x your starters' games per "
                  "week) and over the rest of the season. Market = how the deal looks to them by market value "
                  "(ADP / % rostered worth points, + = in their favour); acceptance is a prior until the harness "
                  "has logged 20 proposals (docs/trades.md).[/]")
    if sweet:
        console.print(_trade_table("Sweet spot: the market calls it fair, our model says you win", sweet,
                                   sweet=True))
    _print_exploits(exploits)
    if wants:
        console.print("[dim]Trade-block wants: " + "; ".join(
            f"{next((t.name for t in lc.teams if t.team_id == k), k)}: {', '.join(sorted(v))}"
            for k, v in wants.items()) + "[/]")
    _print_future(future)
    _footer(lc, errors)


def _trade_offered(provider: Any) -> dict[str, set[str]] | None:
    """team_id -> player cids that team offers on its Fantrax trade block (None without blocks)."""
    out: dict[str, set[str]] = {}
    for b in getattr(provider, "trade_blocks", None) or []:
        tid = str(getattr(b, "team_id", "") or "")
        cids = {c for c in getattr(b, "players_offered", None) or [] if c}
        if tid and cids:
            out[tid] = cids
    return out or None


def _print_exploits(exploits: list[Recommendation]) -> None:
    """The "Exploit" section: one "Exploit: <team> has <pressure>" heading per deal, then the table."""
    if not exploits:
        return
    console.print("[bold]Exploit[/] (teams under roster pressure; relieving it counts +8 market points toward "
                  "acceptance)")
    for i, r in enumerate(exploits, 1):
        text = next((x.text for x in r.reasons if x.code == "EXPLOIT"), "")
        console.print(f"  {i}. {text}")
    console.print(_trade_table("Exploit trades, ranked by expected value", exploits, sweet=True))
    console.print("[dim]Not checked: other teams' moves left this week (providers only report your own "
                  "transaction counter).[/]")


def trade_gain_text(r: Recommendation) -> str:
    """"+0.66/g · +2.3/wk · +55/season" (lineup FPG, pts/week, pts rest of season) for a trade
    rec; only the per-game part when the week / season reasons are missing."""
    fpg = _reason_val(r, "DELTA_ME")
    wk, ssn = _reason_val(r, "GAIN_WEEK"), _reason_val(r, "GAIN_SEASON")
    parts = [f"{_fmt(fpg, signed=True)}/g" if fpg is not None else "-"]
    if wk is not None:
        parts.append(f"{wk:+.1f}/wk")
    if ssn is not None:
        parts.append(f"{ssn:+.0f}/season")
    return " · ".join(parts)


def _trade_table(title: str, recs: list[Recommendation], sweet: bool = False) -> Table:
    """One row per trade: your gain, the market view, acceptance, EV, strength and the why."""
    tbl = Table(title=title, show_lines=True)
    for c, j in (("#", "right"), ("With", "left"), ("Give", "left"), ("Get", "left"), ("You gain", "right"),
                 ("Market", "right"), ("Accept", "right"), ("EV", "right"), ("Strength", "right"),
                 ("Why", "left")):
        tbl.add_column(c, justify=j)
    for i, r in enumerate(recs, 1):
        edge = _reason_val(r, "MY_EDGE")
        dyn = _reason_val(r, "DYNASTY_DELTA")
        gain = trade_gain_text(r) + (f"\ndyn {dyn:+.2f}" if dyn is not None else "")
        perceived, p = _reason_val(r, "MARKET_VIEW"), _reason_val(r, "P_ACCEPT")
        mv = _reason(r, "MARKET_VIEW")
        label = mv.text.split(" by market value")[0].replace("Looks ", "").replace(" to them", "") if mv else ""
        ev = edge * p if edge is not None and p is not None else None
        codes = ("THEIR_NEED", "TRADE_BLOCK", "ROSTER_CONSEQUENCE", "POSITION_CAP", "WIN_NOW_COST",
                 "ROTO_BALANCE", "MOVE_BUDGET")
        why = [x.text for x in r.reasons if x.code in codes]
        if not sweet and _reason(r, "SWEET_SPOT") is not None:
            why.insert(0, "Sweet spot")
        tbl.add_row(str(i) if sweet else _rank_cell(r, i), r.counterparty or "-",
                    "\n".join(f"{x.name} ({_pos(x)})" for x in r.drop),
                    "\n".join(f"{x.name} ({_pos(x)})" for x in r.add), gain,
                    "-" if perceived is None else f"{perceived:+.1f}\n{label}",
                    "-" if p is None else f"{p:.0%}", _fmt(ev, signed=True), _strength_cell(r),
                    "\n".join(why) or "-")
    return tbl


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


@app.command("alerts")
def alerts_cmd(ctx: typer.Context,
               limit: int = typer.Option(25, "--limit", "-n", help="Max alerts."),
               league: Optional[LeagueName] = typer.Option(None, "--league", "-l"),
               json_out: bool = typer.Option(False, "--json")) -> None:
    """Every alert engine: line / PP-unit changes and confirmed goalie starts (Daily Faceoff),
    role changes (NHL TOI / PP share) and rising free agents (% rostered)."""
    from .recommend.advise import advise

    league_name, as_json = _opts(ctx, league, json_out)
    d = _load_all(league_name)
    lc, errors = d.lc, d.errors
    with _collect_logs("fantasy_manager.recommend.advise", errors, "alerts"):
        recs = _safe(errors, "alerts", advise, lc, d.values, dynasty_values=d.dyn, include=("alerts",),
                     limit=limit, default=[]) or []
    if as_json:
        _json_out({"alerts": _recs_json(recs)}, lc, errors)
        return
    if recs:
        console.print(_rec_table(recs, f"Alerts ({len(recs)})"))
    else:
        console.print("[green]No alerts: no line, power-play, role or goalie-start changes worth acting on.[/]")
    _footer(lc, errors)


@app.command("trending")
def trending_cmd(ctx: typer.Context,
                 fallers: bool = typer.Option(False, "--fallers", help="Biggest % rostered drops instead of rises."),
                 limit: int = typer.Option(25, "--limit", "-n", help="Players to list."),
                 league: Optional[LeagueName] = typer.Option(None, "--league", "-l"),
                 json_out: bool = typer.Option(False, "--json")) -> None:
    """ESPN-wide % rostered risers (or fallers) this week, from the public ESPN player pool, tagged
    free agent / rostered when they are in the chosen league."""
    from .matching.normalize import normalize_name
    from .providers.espn import EspnProvider

    league_name, as_json = _opts(ctx, league, json_out)
    settings_ = get_settings()
    cache = _cache()
    errors: list[str] = []
    lc: LeagueContext | None = None
    try:
        lc, provider = _load_context(league_name, cache)
    except typer.Exit:
        provider = None
    espn = provider if isinstance(provider, EspnProvider) else EspnProvider(settings_, cache)
    rows = _safe(errors, "ESPN trending", espn.trending, limit=limit, fallers=fallers, default=[]) or []
    errors.extend(str(w) for w in getattr(espn, "warnings", []) or [] if "trending" in str(w).lower())
    if lc is not None and lc.provider != "espn":        # tag by name + team in a non-ESPN league
        fa = {normalize_name(p.name) for p in lc.free_agents}
        rostered = {normalize_name(p.name) for t in lc.teams for p in t.players}
        for r in rows:
            n = normalize_name(r["name"])
            r["in_league"] = "fa" if n in fa else "rostered" if n in rostered else None
    if as_json:
        _json_out({"fallers": fallers, "players": rows}, lc, errors)
        return
    what = "fallers" if fallers else "risers"
    tbl = Table(title=f"ESPN % rostered {what} (all ESPN leagues, last 7 days)")
    for c, j in (("#", "right"), ("Player", "left"), ("Pos", "left"), ("NHL", "left"), ("Status", "left"),
                 ("% ros", "right"), ("Change", "right"), ("% start", "right"), ("In my league", "left")):
        tbl.add_column(c, justify=j)
    for i, r in enumerate(rows, 1):
        where = {"fa": "[green]free agent[/]", "rostered": "rostered"}.get(r.get("in_league") or "", "-")
        pct = r.get("pct_owned")
        tbl.add_row(str(i), escape(r["name"]), "/".join(x for x in r.get("positions") or [] if x != "F") or "-",
                    r.get("team") or "-", r.get("status") or "-", "-" if pct is None else f"{pct:.1f}",
                    f"{r['pct_owned_change']:+.1f}", "-" if r.get("pct_started") is None else f"{r['pct_started']:.1f}",
                    where)
    console.print(tbl if rows else f"No ESPN {what} available.")
    console.print("[dim]Source: ESPN public player pool (fantasy.espn.com, cached 6h).[/]")
    if lc is not None:
        _footer(lc, errors)
    else:
        for e in errors:
            console.print(f"[dim yellow]! {escape(e)}[/]")


def _dfo_client(cache: HttpCache) -> Any:
    from .providers.dailyfaceoff import DailyFaceoffClient, make_cached_fetch_text

    return DailyFaceoffClient(fetch_text=make_cached_fetch_text(cache))


def _unit_label(x: str | None) -> str:
    return x.upper() if x else "-"


@app.command("lines")
def lines_cmd(ctx: typer.Context,
              team: Optional[str] = typer.Argument(None, help="NHL team (e.g. EDM); default: my players."),
              league: Optional[LeagueName] = typer.Option(None, "--league", "-l"),
              json_out: bool = typer.Option(False, "--json")) -> None:
    """Daily Faceoff line combinations and power-play units: one NHL team's lines, or every one of
    my players' line / PP unit with the change since the last snapshot."""
    from datetime import date as _date

    from .matching.normalize import normalize_name, normalize_team
    from .providers.lines_enrich import LineSnapshotStore, build_snapshot, diff_snapshots

    league_name, as_json = _opts(ctx, league, json_out)
    if team:
        abbrev = normalize_team(team)
        from .providers.dailyfaceoff import TEAM_SLUGS
        if abbrev not in TEAM_SLUGS:
            _fail(f"unknown NHL team {team!r}; use an abbreviation like EDM, TOR or VGK")
        cache = _cache()
        errors: list[str] = []
        tl = _safe(errors, f"Daily Faceoff {abbrev}", _dfo_client(cache).team_lines, abbrev)
        if tl is None:
            _fail(errors[0] if errors else f"Daily Faceoff lines for {abbrev} unavailable")
        today = _date.today()
        store = LineSnapshotStore(get_settings().fm_data_dir)
        prev = store.previous(today)
        changes = diff_snapshots(prev[1], build_snapshot(today, {abbrev: tl})) if prev else {}
        mine: set[str] = set()
        try:
            lc0, _ = _load_context(league_name, cache)
            mine = {normalize_name(p.name) for p in lc0.my_team.players}
        except (typer.Exit, Exception):  # marking my players is optional
            pass
        groups = [("F1", "f1"), ("F2", "f2"), ("F3", "f3"), ("F4", "f4"), ("D1", "d1"), ("D2", "d2"), ("D3", "d3"),
                  ("G", "g"), ("PP1", "pp1"), ("PP2", "pp2"), ("PK1", "pk1"), ("PK2", "pk2"), ("IR / out", "ir")]

        def key(lp: Any) -> str:
            return str(lp.dfo_id) if lp.dfo_id is not None else f"{abbrev}:{normalize_name(lp.name)}"

        if as_json:
            _dump({"team": abbrev, "updated_at": tl.updated_at, "source": tl.source, "source_url": tl.source_url,
                   "previous_snapshot": prev[0].isoformat() if prev else None,
                   "groups": {g: [{"name": lp.name, "position": lp.position, "change": changes.get(key(lp)),
                                   "mine": normalize_name(lp.name) in mine, "injury": lp.injury_status}
                                  for lp in tl.by_group(k)] for g, k in groups if tl.by_group(k)},
                   "errors": errors})
            return
        tbl = Table(title=f"{abbrev} line combinations (Daily Faceoff)", show_lines=True)
        tbl.add_column("Unit")
        tbl.add_column("Players")
        for label, k in groups:
            members = tl.by_group(k)
            if not members:
                continue
            cells = []
            for lp in members:
                name = escape(lp.name)
                if normalize_name(lp.name) in mine:
                    name = f"[bold green]{name}[/]"
                ch = changes.get(key(lp))
                cells.append(name + (f" [yellow]({escape(ch)})[/]" if ch else ""))
            tbl.add_row(label, ", ".join(cells))
        console.print(tbl)
        when = tl.updated_at.strftime("%b %d %H:%M UTC") if tl.updated_at else "update time n/a"
        since = f"; changes vs snapshot {prev[0]}" if prev else "; no earlier snapshot to compare"
        console.print(f"[dim]{DFO_CREDIT}. Source: {escape(tl.source or 'Daily Faceoff')}, updated {when}{since}.[/]")
        for e in errors:
            console.print(f"[dim yellow]! {escape(e)}[/]")
        return

    d = _load_all(league_name)
    lc = d.lc
    players = sorted(lc.my_team.players, key=lambda p: (p.is_goalie, p.line or "zz", p.name))
    rows = [{"cid": p.cid, "name": p.name, "pos": _pos(p), "team": p.team, "line": p.line, "pp_unit": p.pp_unit,
             "pk_unit": p.pk_unit, "line_change": p.line_change, "confirmed_start": p.confirmed_start,
             "start_source": p.start_source, "toi_per_game": p.toi_per_game, "pp_share": p.pp_share}
            for p in players]
    if as_json:
        _json_out({"players": rows}, lc, d.errors)
        return
    tbl = Table(title=f"{lc.my_team.name}: lines and power-play units (Daily Faceoff)")
    for c, j in (("Player", "left"), ("Pos", "left"), ("NHL", "left"), ("Line", "left"), ("PP", "left"),
                 ("PK", "left"), ("TOI/GP", "right"), ("PP share", "right"), ("Change", "left")):
        tbl.add_column(c, justify=j)
    for r in rows:
        ch = r["line_change"]
        line = _unit_label(r["line"])
        if r["line"] == "g" and r["confirmed_start"] is not None:
            line = "G (starts tonight)" if r["confirmed_start"] else "G (not starting)"
        tbl.add_row(escape(r["name"]), r["pos"], r["team"] or "-", line, _unit_label(r["pp_unit"]),
                    _unit_label(r["pk_unit"]), _fmt(r["toi_per_game"]),
                    "-" if r["pp_share"] is None else f"{r['pp_share']:.0%}",
                    f"[yellow]{escape(ch)}[/]" if ch else "-")
    console.print(tbl)
    if not any(r["line"] for r in rows):
        console.print("[dim]No Daily Faceoff lines matched your players (offline, or the pages failed).[/]")
    _footer(lc, d.errors)


@app.command("goalies")
def goalies_cmd(ctx: typer.Context,
                league: Optional[LeagueName] = typer.Option(None, "--league", "-l"),
                json_out: bool = typer.Option(False, "--json")) -> None:
    """Tonight's starting goalies (Daily Faceoff, cached 3h) and whether my goalies start."""
    from .matching.normalize import normalize_name, normalize_team

    league_name, as_json = _opts(ctx, league, json_out)
    d = _load_all(league_name)
    lc, errors = d.lc, d.errors
    starts = _safe(errors, "Daily Faceoff starting goalies", _dfo_client(d.cache).starting_goalies, lc.as_of,
                   default=[]) or []
    mine = [p for p in lc.my_team.players if p.is_goalie]
    mine_names = {normalize_name(p.name) for p in mine}
    mine_rows = []
    for p in mine:
        team = normalize_team(p.team)
        plays = lc.as_of in set(lc.schedule.get(team or "", []))
        listed = next((x for x in starts if normalize_name(x.goalie_name) == normalize_name(p.name)), None)
        state = ("no game tonight" if lc.schedule and not plays else
                 "starting" if p.confirmed_start else "not starting" if p.confirmed_start is False else
                 f"listed, {(listed.strength or 'unconfirmed').lower()}" if listed is not None else "unknown")
        mine_rows.append({"cid": p.cid, "name": p.name, "team": p.team, "plays_tonight": plays if lc.schedule else None,
                          "state": state, "source": p.start_source})
    if as_json:
        _json_out({"day": lc.as_of.isoformat(), "starts": [s.model_dump(mode="json") for s in starts],
                   "my_goalies": mine_rows}, lc, errors)
        return
    tbl = Table(title=f"Starting goalies {lc.as_of:%a %b %d} (Daily Faceoff)")
    for c in ("Game", "Team", "Goalie", "Status", "Reported by"):
        tbl.add_column(c)
    for s in sorted(starts, key=lambda s: (s.game_time or datetime.max.replace(tzinfo=timezone.utc), s.game, s.home)):
        name = escape(s.goalie_name)
        if normalize_name(s.goalie_name) in mine_names:
            name = f"[bold green]{name} (mine)[/]"
        style = "green" if s.is_confirmed else ("yellow" if s.is_start else "dim")
        when = s.created_at.strftime("%H:%M UTC") if s.created_at else ""
        tbl.add_row(s.game, s.team, name, f"[{style}]{escape(s.strength or 'unconfirmed')}[/]",
                    escape(" ".join(x for x in (s.source or "", when) if x)) or "-")
    console.print(tbl if starts else "No starting-goalie reports for today yet.")
    mt = Table(title="My goalies tonight")
    for c in ("Goalie", "NHL", "Tonight", "Source"):
        mt.add_column(c)
    for r in mine_rows:
        style = {"starting": "green", "not starting": "red"}.get(r["state"], "dim")
        mt.add_row(escape(r["name"]), r["team"] or "-", f"[{style}]{r['state']}[/]", escape(r["source"] or "-"))
    console.print(mt)
    console.print("[dim]The starting-goalies page is cached 3h; run again about an hour before puck drop "
                  "for late confirmations.[/]")
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


# -- schedule grid / streaming planner and matchup preview (analysis/) ----------

def _parse_day(text: Optional[str]) -> Any:
    from datetime import date as _date

    if not text:
        return None
    try:
        return _date.fromisoformat(text.strip())
    except ValueError:
        _fail(f"--week expects a date like 2026-10-12, got {text!r}")


def _load_schedule_ctx(league: str, need_values: bool) -> Loaded:
    """provider.load -> NHL enrichment (schedule) [-> valuation when ``need_values``]."""
    if need_values:
        return _load_all(league)
    from .providers.enrich import enrich_context

    cache = _cache()
    lc, provider = _load_context(league, cache)
    try:
        enrich_context(lc, get_settings(), cache)
    except Exception as e:  # enrichment is best effort
        lc.warnings.append(f"NHL enrichment failed: {e}")
    return Loaded(lc, {}, None, provider, cache)


def _week_table(view: Any) -> Table:
    tbl = Table(title=f"NHL schedule, week of {view.start:%a %b %d} - {view.end:%a %b %d} "
                      f"(off-night = fewer than {view.threshold} games, marked *)")
    tbl.add_column("Team")
    for d, n, off in zip(view.days, view.league_games, view.off_days):
        tbl.add_column(f"{d:%a %m/%d}\n{n} gm" + (" *" if off else ""), justify="center")
    for c in ("G", "Off", "B2B"):
        tbl.add_column(c, justify="right")
    for r in view.rows:
        cells = []
        for c in r.cells:
            if c is None:
                cells.append("[dim]-[/]")
                continue
            label = escape(c.opp or "x") + ("*" if c.off else "") + ("^" if c.b2b else "")
            cells.append(f"[bold]{label}[/]" if c.off else label)
        tbl.add_row(r.team, *cells, str(r.games), str(r.offnights), str(r.b2b))
    return tbl


def _stream_tables(plan: Any) -> list[Table]:
    out = []
    teams = Table(title=f"Best NHL teams to stream from ({plan.from_day:%a %b %d} - {plan.week_end:%a %b %d})")
    for c, j in (("Team", "left"), ("Games", "right"), ("Off-night", "right"), ("B2B", "right"),
                 ("Healthy FAs", "right")):
        teams.add_column(c, justify=j)
    for r in plan.teams:
        teams.add_row(r.team, str(r.games), str(r.offnights), str(r.b2b),
                      "-" if r.free_agents is None else str(r.free_agents))
    out.append(teams)
    for slot, targets in plan.by_slot.items():
        tbl = Table(title=f"Streaming targets: {slot} (proj = per-game x games x (1 + {plan.bonus:g} x off-nights))")
        for c, j in (("Player", "left"), ("Pos", "left"), ("NHL", "left"), ("GP", "right"), ("Off", "right"),
                     ("FP/G", "right"), ("Proj", "right"), ("Own%", "right"), ("Opponents", "left")):
            tbl.add_column(c, justify=j)
        for t in targets:
            name = escape(t.name) + ("" if t.status == "healthy" else f" [yellow]({t.status})[/]")
            if t.needs_drop:
                name += f" [magenta]({escape(t.needs_drop)})[/]"
            tbl.add_row(name, t.pos, t.team or "-", str(t.games), str(t.offnights), _fmt(t.per_game), _fmt(t.proj),
                        "-" if t.pct_owned is None else f"{t.pct_owned:.0f}", escape(" ".join(t.opps)))
        if not targets:
            tbl.add_row("[dim](no free agents with games)[/]", "", "", "", "", "", "", "", "")
        out.append(tbl)
    return out


def _playoff_table(po: Any, limit: int | None = None) -> Table:
    heads = [f"P{p.number}\n{p.start:%m/%d}" for p in po.periods]
    tbl = Table(title=f"Fantasy playoffs: games per NHL team (periods {', '.join(str(p.number) for p in po.periods)}; "
                      f"{po.source})")
    tbl.add_column("#", justify="right")
    tbl.add_column("Team")
    for h in heads:
        tbl.add_column(h, justify="right")
    for c in ("Total", "Off-night", "B2B", "vs avg"):
        tbl.add_column(c, justify="right")
    for i, t in enumerate(po.teams[:limit] if limit else po.teams, 1):
        tbl.add_row(str(i), t.team, *[str(g) for g in t.games], str(t.total), str(t.total_offnights),
                    str(t.total_b2b), f"{t.total - po.avg_total:+.1f}")
    return tbl


def _season_table(grid: Any) -> Table:
    tbl = Table(title="Games per NHL team per fantasy week (Mon-Sun)")
    tbl.add_column("Team")
    for w in grid.weeks:
        tbl.add_column(f"{w:%m/%d}", justify="right")
    tbl.add_column("Total", justify="right")
    for t in grid.teams:
        tbl.add_row(t.team, *[str(g) for g in t.games], str(t.total_games))
    tbl.add_row("[dim]NHL games[/]", *[f"[dim]{n}[/]" for n in grid.league_games], "")
    return tbl


@app.command("schedule")
def schedule_cmd(ctx: typer.Context,
                 week: Optional[str] = typer.Option(None, "--week", help="Any date in the week (YYYY-MM-DD); "
                                                                         "default: this week."),
                 playoffs: bool = typer.Option(False, "--playoffs", help="Games per NHL team in the fantasy playoffs."),
                 stream: bool = typer.Option(False, "--stream", help="Free agents to stream this week, by slot."),
                 season: bool = typer.Option(False, "--season", help="Season-long team x week game counts."),
                 limit: int = typer.Option(10, "--limit", help="Streaming targets per slot."),
                 league: Optional[LeagueName] = typer.Option(None, "--league", "-l"),
                 json_out: bool = typer.Option(False, "--json")) -> None:
    """NHL schedule grid for a fantasy week (off-nights, back-to-backs), streaming targets and
    fantasy-playoff schedule strength."""
    from .analysis.schedule_grid import (default_week, playoff_weeks, season_grid, streaming_targets,
                                         week_rows)

    league_name, as_json = _opts(ctx, league, json_out)
    day = _parse_day(week)
    d = _load_schedule_ctx(league_name, need_values=stream)
    lc = d.lc
    if not lc.schedule:
        lc.warnings.append("No NHL schedule loaded (offline or the schedule request failed).")
    start = day or default_week(lc)
    view = week_rows(lc, start)
    plan = streaming_targets(lc, d.values, view.start, limit=limit) if stream else None
    if plan is None and lc.schedule:  # the team half of the streaming plan needs no values
        plan = streaming_targets(lc, {}, view.start, limit=0)
    po = playoff_weeks(lc, d.provider) if playoffs else None
    grid = season_grid(lc) if season else None
    if as_json:
        _dump({"week": view.model_dump(mode="json"),
               "streaming": plan.model_dump(mode="json") if plan else None,
               "playoffs": po.model_dump(mode="json") if po else None,
               "season": grid.model_dump(mode="json") if grid else None, "meta": _meta(lc)})
        return
    if lc.schedule:
        console.print(_week_table(view))
        console.print("[dim]* off-night game (fewer than 8 NHL games that night); ^ second night of a "
                      "back-to-back; @XXX = away.[/]")
    if plan is not None:
        tables = _stream_tables(plan)
        console.print(tables[0])
        if stream:
            for t in tables[1:]:
                console.print(t)
    if po is not None:
        for n in po.notes:
            console.print(f"[dim]{escape(n)}[/]")
        if po.teams:
            console.print(_playoff_table(po))
    if grid is not None and grid.teams:
        console.print(_season_table(grid))
    _footer(lc)


@app.command("matchup")
def matchup_cmd(ctx: typer.Context,
                seed: Optional[int] = typer.Option(None, "--seed", help="Random seed for the win-probability draws."),
                league: Optional[LeagueName] = typer.Option(None, "--league", "-l"),
                json_out: bool = typer.Option(False, "--json")) -> None:
    """This period's head-to-head matchup: score, projected remaining points, win probability and advice."""
    from .analysis.matchup import current_matchup

    league_name, as_json = _opts(ctx, league, json_out)
    d = _load_all(league_name)
    lc = d.lc
    m = current_matchup(lc, d.provider, d.values, seed=seed)
    if as_json:
        _dump({**m.to_json(), "meta": _meta(lc)})
        return
    when = f"period {m.period}" if m.period else "this period"
    if m.start and m.end:
        when += f", {m.start:%a %b %d} - {m.end:%a %b %d} ({m.days_left} days left)"
    console.print(f"[bold]{escape(m.my_team)}[/] vs [bold]{escape(m.opponent_team or 'unknown opponent')}[/]"
                  f" - {when}{' (playoffs)' if m.playoffs else ''}")
    tbl = Table(title="Matchup")
    for c in ("", "So far", "Proj remaining", "Proj total", "Games left", "SD"):
        tbl.add_column(c, justify="right" if c else "left")
    tbl.add_row(escape(m.my_team), _fmt(m.my_points_so_far), _fmt(m.my_projected_remaining),
                _fmt(m.my_projected_total), str(m.my_games_left), _fmt(m.my_sd))
    if m.opponent_team:
        tbl.add_row(escape(m.opponent_team), _fmt(m.their_points_so_far), _fmt(m.their_projected_remaining),
                    _fmt(m.their_projected_total), str(m.their_games_left), _fmt(m.their_sd))
    console.print(tbl)
    if m.win_probability is not None:
        console.print(f"Win probability: [bold]{m.win_probability:.0%}[/] ({m.draws} draws) - "
                      f"stance: [bold]{m.stance}[/]   margin {m.margin:+.1f}")
    if m.gap_by_position:
        gt = Table(title="Projected remaining by position")
        for c in ("Pos", "Mine", "Theirs", "Gap"):
            gt.add_column(c, justify="left" if c == "Pos" else "right")
        for g, x in m.gap_by_position.items():
            gt.add_row(g, _fmt(x.mine), _fmt(x.theirs), _fmt(x.gap, True))
        console.print(gt)
    if m.key_players_theirs:
        kt = Table(title="Their key players")
        for c in ("Player", "Tag", "Pos", "NHL", "Status", "GP left", "Proj", "Note"):
            kt.add_column(c, justify="right" if c in ("GP left", "Proj") else "left")
        for k in m.key_players_theirs:
            kt.add_row(escape(k.name), k.tag, k.pos, k.team or "-", k.status, str(k.games_left),
                       _fmt(k.proj_remaining), escape(k.note or ""))
        console.print(kt)
    for a in m.advice:
        console.print(f"- {escape(a)}")
    console.print(f"[dim]Lineups: {escape(m.lineup_basis)}; scores: {escape(m.source)}.[/]")
    for w in m.warnings:
        console.print(f"[dim yellow]! {escape(w)}[/]")
    _footer(lc)


if __name__ == "__main__":
    app()
