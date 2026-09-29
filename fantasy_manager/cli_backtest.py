"""`fm backtest ...`: history table, model evaluation, parameter fitting, projection archive.

Mounted into the main app with ``app.add_typer(backtest_app, name="backtest")``.
"""
from __future__ import annotations

import json
import time
from datetime import date
from pathlib import Path
from typing import Any, Optional

import typer
from rich.console import Console
from rich.markup import escape
from rich.table import Table

backtest_app = typer.Typer(help="Backtest projection models on NHL history, fit constants, archive projections.",
                           no_args_is_help=True)
console = Console(emoji=False)


def _settings():
    from .config import get_settings
    return get_settings()


def _cache(settings):
    from .cache import HttpCache
    return HttpCache(settings.fm_data_dir, offline=settings.fm_offline)


@backtest_app.command("data")
def data_cmd(from_season: str = typer.Option("20102011", "--from-season", help="First season (e.g. 20102011 or 2010)."),
             to_season: Optional[str] = typer.Option(None, "--to-season", help="Last season (default: last finished)."),
             inseason: str = typer.Option("20212022-20252026", "--inseason",
                                          help="Seasons to fetch Nov 1 / Dec 1 / Jan 1 checkpoint windows for "
                                               "('' to skip)."),
             force: bool = typer.Option(False, "--force", help="Refetch seasons already stored.")) -> None:
    """Build / refresh the per-player-season table (incremental; cached for 30 days)."""
    from .backtest.data import LAST_SEASON, parse_seasons, season_range
    from .backtest.pipeline import refresh_data

    first = parse_seasons(from_season, [])[0]
    last = parse_seasons(to_season, [LAST_SEASON])[-1]
    settings = _settings()
    cache = _cache(settings)
    try:
        info = refresh_data(cache, settings.fm_data_dir, season_range(first, last),
                            parse_seasons(inseason, []) if inseason else [], force=force,
                            log=lambda m: console.print(escape(m)))
    finally:
        cache.close()
    console.print(f"{info['rows']} player-seasons over {len(info['seasons'])} seasons, "
                  f"{info['checkpoints']} in-season checkpoints. {info['network_requests']} network requests "
                  f"({info['network_seconds']}s), {info['cache_hits']} cache hits, wall {info['wall_seconds']}s.")


@backtest_app.command("run")
def run_cmd(models: Optional[str] = typer.Option(None, "--models", help="Comma list: naive,marcel,fm_current,fm_fitted,fm_multi."),
            seasons: Optional[str] = typer.Option(None, "--seasons", help="e.g. 20162017-20252026 (default)."),
            scoring: str = typer.Option("espn", "--scoring", help="espn | fantrax | default | path to JSON."),
            inseason: bool = typer.Option(True, "--inseason/--no-inseason", help="Also score in-season checkpoints."),
            json_out: bool = typer.Option(False, "--json")) -> None:
    """Evaluate the models; writes results-<date>.json and report-<date>.md under data/backtest."""
    from .backtest.data import parse_seasons
    from .backtest.evaluate import summarize
    from .backtest.pipeline import run_evaluation

    settings = _settings()
    model_list = [m.strip() for m in models.split(",") if m.strip()] if models else None
    out = run_evaluation(settings.fm_data_dir, scoring, model_list, parse_seasons(seasons, []) or None,
                         inseason=inseason, log=lambda m: None if json_out else console.print(escape(m)))
    if json_out:
        typer.echo(json.dumps({"results": out["results"], "report": out["report"],
                               "summary": {f"{k}/{p}": summarize(out["rows"], k, p)
                                           for k in ("preseason", "inseason") for p in ("skaters", "goalies")}},
                              indent=2, default=str))
        return
    for kind, pool in (("preseason", "skaters"), ("preseason", "goalies"), ("inseason", "skaters")):
        s = summarize(out["rows"], kind, pool)
        if not s:
            continue
        t = Table(title=f"{kind} - {pool} (mean over seasons)")
        for c in ("model", "MAE", "RMSE", "bias", "Spearman", "top-50", "top-100", "n"):
            t.add_column(c, justify="left" if c == "model" else "right")
        for m, v in s.items():
            f = lambda x, fmt="{:.3f}": "-" if x is None else fmt.format(x)
            t.add_row(m, f(v["mae"]), f(v["rmse"]), f(v["bias"]), f(v["spearman"]),
                      f(v["top50"], "{:.0%}"), f(v["top100"], "{:.0%}"), str(int(v["n"])))
        console.print(t)
    console.print(f"Report: {out['report']}\nResults: {out['results']}")


@backtest_app.command("fit")
def fit_cmd(scoring: str = typer.Option("espn", "--scoring"),
            boot: int = typer.Option(200, "--boot", help="Bootstrap replicates for the age-curve intervals."),
            json_out: bool = typer.Option(False, "--json")) -> None:
    """Fit shrinkage k, recency weights and age curves; writes data/backtest/fitted_params.json.

    The app uses the packaged fantasy_manager/valuation/fitted_params.json; promote a new fit by
    copying it there (the command prints how)."""
    from .backtest.fit import REPORT_AGES
    from .backtest.pipeline import run_fit

    settings = _settings()
    out = run_fit(settings.fm_data_dir, scoring, boot=boot)
    if json_out:
        typer.echo(json.dumps(out, indent=2, default=str))
        return
    pre = out["preseason"]
    ak, km = pre["app_k"], pre["k_multi"]
    console.print(f"[bold]Preseason shrinkage k[/], 3 seasons (the app's baseline): F {km['F']:.0f}, D {km['D']:.0f}, "
                  f"G {km['G']:.0f} (app {ak['F']:g}/{ak['D']:g}/{ak['G']:g}); 1 season: F {pre['k']['F']:.0f}, "
                  f"D {pre['k']['D']:.0f}, G {pre['k']['G']:.0f}, skaters pooled {pre['k_skater_pooled']:.0f}")
    ins = out.get("inseason")
    if ins:
        w = ins["recency_weights"]
        console.print(f"[bold]In-season[/]: k skater {ins['k_skater']:.0f}, goalie {ins['k_goalie']:.0f}; weights "
                      f"season {w['season']:.2f} / L30 {w['last30']:.2f} / L15 {w['last15']:.2f} / L7 {w['last7']:.2f}"
                      f"  (MAE {ins['mae_app']:.4f} -> {ins['mae_fitted']:.4f})")
    t = Table(title="Age curve (level, peak = 1.00, 90% CI)")
    t.add_column("group")
    for a in REPORT_AGES:
        t.add_column(str(a), justify="right")
    for g, ages in out["age_curve_report"].items():
        t.add_row(g, *(f"{ages[str(a)]['level']:.2f}" for a in REPORT_AGES))
        t.add_row("CI", *(f"{ages[str(a)]['lo']:.2f}-{ages[str(a)]['hi']:.2f}" for a in REPORT_AGES))
        t.add_row("n", *(str(ages[str(a)]['n']) for a in REPORT_AGES))
    console.print(t)
    console.print(f"Wrote {out['path']}")
    console.print("The app reads the packaged copy, which this command never overwrites. To adopt this fit:",
                  markup=False)
    console.print(f"  copy \"{out['path']}\" \"{out['packaged_path']}\"", markup=False, soft_wrap=True)
    console.print("then run the tests and `fm backtest run` (fm_current should match fm_multi).", markup=False)


def _load_league(league: str, settings: Any, cache: Any, with_recs: bool) -> tuple[Any, Any, list, list[str]]:
    """provider -> enrich -> valuate (-> advise): the web loader's building blocks, minus news."""
    ctx, values, recs, warnings, _ = _load_league_full(league, settings, cache, with_recs)
    return ctx, values, recs, warnings


def _load_league_full(league: str, settings: Any, cache: Any, with_recs: bool
                      ) -> tuple[Any, Any, list, list[str], Any]:
    """_load_league plus the provider (the harness pulls activity / lineups from it)."""
    from .providers import get_provider
    from .providers.enrich import enrich_context
    from .scoring import fit_to_context, from_config
    from .valuation.valuate import valuate_league

    provider = get_provider(league, settings, cache)
    ctx = provider.load()
    warnings = [str(w) for w in getattr(provider, "warnings", None) or []]
    try:
        enrich_context(ctx, settings, cache)
    except Exception as e:  # best effort, as in the web loader
        warnings.append(f"NHL enrichment failed: {e}")
    values = valuate_league(ctx, fit_to_context(from_config(ctx.scoring), ctx))
    recs: list = []
    if with_recs:
        try:
            from .recommend.advise import advise
            from .recommend.injuries import StatusHistory

            dyn = None
            if ctx.dynasty:
                try:
                    from .valuation.dynasty import apply_dynasty
                    ages = getattr(provider, "ages", None)
                    dyn = apply_dynasty(values, ctx, ages=ages if isinstance(ages, dict) and ages else None)
                except Exception as e:
                    warnings.append(f"dynasty valuation failed: {e}")
            history = StatusHistory(settings.fm_data_dir)
            try:  # read-only use, like the web loader
                recs = advise(ctx, values, dynasty_values=dyn, history=history)
            finally:
                history.close()
        except Exception as e:
            warnings.append(f"recommendations failed: {e}")
    return ctx, values, recs, warnings, provider


@backtest_app.command("archive")
def archive_cmd(league: str = typer.Option("all", "--league", "-l", help="espn | fantrax | all"),
                recs: bool = typer.Option(True, "--recs/--no-recs", help="Also archive recommendations.")) -> None:
    """Snapshot provider projections (+ ours) and recommendations to data/archive (daily-safe)."""
    from .backtest.archive import archive_projections, archive_recommendations
    from .providers.base import ProviderError

    settings = _settings()
    leagues = ["espn", "fantrax"] if league == "all" else [league]
    failed = 0
    for lg in leagues:
        cache = _cache(settings)
        t0 = time.perf_counter()
        try:
            ctx, values, rec_list, warnings = _load_league(lg, settings, cache, recs)
        except ProviderError as e:
            console.print(f"[yellow]{lg}: skipped ({escape(str(e))})[/]")
            failed += league != "all"
            continue
        except Exception as e:
            console.print(f"[red]{lg}: failed to load ({escape(type(e).__name__)}: {escape(str(e))})[/]")
            failed += 1
            continue
        finally:
            cache.close()
        path, status = archive_projections(ctx, settings.fm_data_dir, values)
        n_proj = sum(1 for p in ctx.all_players() if "projected" in p.lines)
        console.print(f"{lg}: projections {status} -> {path.name} ({len(ctx.all_players())} players, "
                      f"{n_proj} with a provider projection)")
        if recs:
            rpath, rstatus = archive_recommendations(rec_list, ctx.provider, settings.fm_data_dir,
                                                     as_of=ctx.as_of, league_id=ctx.league_id)
            console.print(f"{lg}: recommendations {rstatus} -> {rpath.name} ({len(rec_list)} recs)")
        for w in warnings:
            console.print(f"  [dim]{escape(w)}[/]")
        console.print(f"  [dim]{time.perf_counter() - t0:.1f}s[/]")
    if failed:
        raise typer.Exit(1)


@backtest_app.command("grade")
def grade_cmd(season: str = typer.Option(..., "--season", help="Finished season to grade, e.g. 20262027."),
              which: str = typer.Option("first", "--snapshot", help="first | last | YYYY-MM-DD"),
              scoring: Optional[str] = typer.Option(None, "--scoring", help="Grade all under one preset.")) -> None:
    """Score archived provider projections vs ours against a finished season's actuals."""
    from .backtest.archive import score_archive
    from .backtest.data import load_table, parse_seasons, season_start_year
    from .backtest.scoring import load_scoring

    settings = _settings()
    s = parse_seasons(season, [])[0]
    table = load_table(settings.fm_data_dir)
    actuals = table.by_season.get(s)
    if not actuals:
        console.print(f"[red]No stored actuals for {s}; run `fm backtest data --to-season {s}` first.[/]")
        raise typer.Exit(1)
    y = season_start_year(s)
    cfg = load_scoring(scoring)[1] if scoring else None
    out = score_archive(actuals, settings.fm_data_dir, which=which, scoring=cfg,
                        start=date(y, 7, 1), end=date(y + 1, 6, 30))
    typer.echo(json.dumps(out, indent=2, default=str))


@backtest_app.command("report")
def report_cmd() -> None:
    """Print the latest backtest report."""
    from .backtest.pipeline import latest_report

    p = latest_report(_settings().fm_data_dir)
    if p is None:
        console.print("No report yet; run `fm backtest run`.")
        raise typer.Exit(1)
    console.print(f"[dim]{p}[/]")
    typer.echo(Path(p).read_text(encoding="utf-8"))
