"""`fm harness ...`: capture and match recommendations against what happened (M1), grade them
and keep the bar (M2: ``grade``, ``status``).

Mounted into the main app with ``app.add_typer(harness_app, name="harness")``. See
docs/harness.md.
"""
from __future__ import annotations

import json
import time
from datetime import date, timedelta
from typing import Any, Optional

import typer
from rich.console import Console
from rich.markup import escape
from rich.table import Table

harness_app = typer.Typer(help="Evaluation harness: capture recommendations, moves and results; match and "
                               "grade them.",
                          no_args_is_help=True)
console = Console(emoji=False)

LEAGUES = ("espn", "fantrax")
TX_LOOKBACK_DAYS = 7


def _settings():
    from .config import get_settings
    return get_settings()


def _cache(settings):
    from .cache import HttpCache
    return HttpCache(settings.fm_data_dir, offline=settings.fm_offline)


def _ledger(settings):
    from .harness.ledger import Ledger
    return Ledger(settings.fm_data_dir)


def _leagues(league: str) -> list[str]:
    league = (league or "all").lower()
    if league in ("all", "both"):
        return list(LEAGUES)
    if league not in LEAGUES:
        console.print(f"[red]Unknown league {escape(league)!r}; expected espn, fantrax or all.[/]")
        raise typer.Exit(2)
    return [league]


def _parse_day(s: Optional[str]) -> date | None:
    if not s:
        return None
    try:
        return date.fromisoformat(s)
    except ValueError:
        console.print(f"[red]Bad date {escape(s)!r}; use YYYY-MM-DD.[/]")
        raise typer.Exit(2)


def _archived(data_dir: Any, league: str, day: date) -> bool:
    from .backtest.archive import archive_dir

    d = archive_dir(data_dir)
    return all((d / f"{k}-{league}-{day.isoformat()}.json").exists() for k in ("projections", "recs"))


def _daily_league(lg: str, settings: Any, ledger: Any, today: date, since: date, archive: bool,
                  force_archive: bool) -> dict[str, Any]:
    """One league's daily capture; returns a summary dict (never raises for provider issues)."""
    from .backtest.archive import archive_projections, archive_recommendations
    from .cli_backtest import _load_league_full
    from .harness.ingest import ingest_archive
    from .harness.match import match_episodes
    from .harness.realized import pull_lineups, pull_transactions
    from .providers.base import ProviderError

    out: dict[str, Any] = {"league": lg, "warnings": []}
    need_archive = archive and (force_archive or not _archived(settings.fm_data_dir, lg, today))
    cache = _cache(settings)
    ctx = provider = None
    try:
        ctx, values, recs, warnings, provider = _load_league_full(lg, settings, cache, need_archive)
        out["warnings"].extend(warnings)
    except ProviderError as e:
        out["skipped"] = str(e)
    except Exception as e:  # noqa: BLE001 - one league must never sink the other
        out["skipped"] = f"{type(e).__name__}: {e}"
    try:
        if ctx is not None and need_archive:
            _, pst = archive_projections(ctx, settings.fm_data_dir, values)
            _, rst = archive_recommendations(recs, ctx.provider, settings.fm_data_dir, as_of=ctx.as_of,
                                             league_id=ctx.league_id)
            out["archive"] = f"projections {pst}, recs {rst} ({len(recs)} recs)"
        elif archive:
            out["archive"] = "already archived today" if ctx is not None or _archived(settings.fm_data_dir, lg, today) \
                else "not archived (league unavailable)"
        if ctx is not None:
            from .harness.outcomes import store_scoring
            store_scoring(ledger, lg, ctx.scoring)
        ing = ingest_archive(ledger, settings.fm_data_dir, [lg])
        out["ingest"] = {k: ing[k] for k in ("files_ingested", "projections", "recs", "episodes")}
        out["warnings"].extend(ing["errors"])
        if ctx is not None and provider is not None:
            n_warn = len(getattr(provider, "warnings", []) or [])
            out["transactions"] = pull_transactions(ledger, provider, lg, since, ctx)
            lineup_day = today if lg == "fantrax" else today - timedelta(days=1)
            out["lineups"] = {"day": lineup_day.isoformat(),
                              "rows": pull_lineups(ledger, provider, lg, lineup_day, ctx, today)}
            out["warnings"].extend((getattr(provider, "warnings", []) or [])[n_warn:])
        m = match_episodes(ledger, lg, today)
        out["match"] = {"episodes": m.episodes, "by_status": m.by_status, "decisions": m.decisions}
    finally:
        cache.close()
    return out


def last_monday(day: date) -> date:
    """The most recent Monday on or before ``day`` (grading week)."""
    return day - timedelta(days=day.weekday())


def _grade_leagues(ledger: Any, leagues: list[str], week: date, today: date) -> list[dict[str, Any]]:
    """Grade matured outcomes (as of ``today``) then the bar for ``week``, per league."""
    from .harness.metrics import grade_week
    from .harness.outcomes import grade_outcomes

    out = []
    for lg in leagues:
        r: dict[str, Any] = {"league": lg}
        try:
            g = grade_outcomes(ledger, lg, today)
            r["outcomes"] = g.to_dict()
            r["outcomes_line"] = g.line()
            r["week"] = grade_week(ledger, lg, week)
        except Exception as e:  # noqa: BLE001 - one league must never sink the other
            r["error"] = f"{type(e).__name__}: {e}"
        out.append(r)
    return out


@harness_app.command("daily")
def daily_cmd(league: str = typer.Option("all", "--league", "-l", help="espn | fantrax | all"),
              archive: bool = typer.Option(True, "--archive/--no-archive",
                                           help="Archive today's projections + recs first when not done yet."),
              force_archive: bool = typer.Option(False, "--force-archive", help="Re-archive even if done today."),
              since: Optional[str] = typer.Option(None, "--since",
                                                  help=f"Transactions since (default: {TX_LOOKBACK_DAYS} days ago)."),
              realized_day: Optional[str] = typer.Option(None, "--realized-day",
                                                         help="NHL game date to pull (default: yesterday)."),
              grade: bool = typer.Option(False, "--grade",
                                         help="Also grade outcomes and this week's bar (always on Mondays)."),
              json_out: bool = typer.Option(False, "--json")) -> None:
    """Idempotent daily capture: archive -> ingest -> transactions / lineups / NHL results -> match
    (-> grade on Mondays or with --grade)."""
    from .backtest.data import CountingFetcher
    from .harness.ingest import record_run
    from .harness.realized import pull_realized
    from .providers.nhl import NhlClient

    settings = _settings()
    today = date.today()
    since_day = _parse_day(since) or today - timedelta(days=TX_LOOKBACK_DAYS)
    rday = _parse_day(realized_day) or today - timedelta(days=1)
    t0 = time.perf_counter()
    ledger = _ledger(settings)
    results: list[dict[str, Any]] = []
    try:
        for lg in _leagues(league):
            results.append(_daily_league(lg, settings, ledger, today, since_day, archive, force_archive))
        cache = _cache(settings)
        realized: dict[str, Any] = {"day": rday.isoformat()}
        try:
            realized["players"] = pull_realized(ledger, NhlClient(fetch_json=CountingFetcher(cache)), rday)
        except Exception as e:  # noqa: BLE001 - NHL API hiccups must not fail the run
            realized["error"] = f"{type(e).__name__}: {e}"
        finally:
            cache.close()
        graded = None
        if grade or today.weekday() == 0:
            graded = _grade_leagues(ledger, _leagues(league), last_monday(today), today)
        summary = {"leagues": results, "realized": realized, "grade": graded,
                   "seconds": round(time.perf_counter() - t0, 1)}
        failed = [r["league"] for r in results if r.get("skipped")]
        record_run(ledger, "daily", league, today, summary, "partial" if failed or "error" in realized else "ok")
    finally:
        ledger.close()
    if json_out:
        typer.echo(json.dumps(summary, indent=2, default=str))
        return
    for r in results:
        lg = r["league"]
        if r.get("skipped"):
            console.print(f"[yellow]{lg}: league not loaded ({escape(str(r['skipped']))})[/]")
        if r.get("archive"):
            console.print(f"{lg}: archive {escape(r['archive'])}")
        ing = r.get("ingest") or {}
        console.print(f"{lg}: ingest {ing.get('files_ingested', 0)} new file(s), {ing.get('episodes', 0)} episodes")
        if "transactions" in r:
            ln = r.get("lineups") or {}
            console.print(f"{lg}: {r['transactions']} transaction rows since {since_day}, "
                          f"{ln.get('rows', 0)} lineup rows for {ln.get('day')}")
        m = r.get("match") or {}
        st = ", ".join(f"{k} {v}" for k, v in sorted((m.get("by_status") or {}).items())) or "none"
        dec = ", ".join(f"{k} {v}" for k, v in sorted((m.get("decisions") or {}).items())) or "none"
        console.print(f"{lg}: episodes {st}; decisions {dec}")
        for w in r.get("warnings") or []:
            console.print(f"  [dim]{escape(str(w))}[/]")
    rl = summary["realized"]
    if "error" in rl:
        console.print(f"[yellow]NHL results {rl['day']}: {escape(rl['error'])}[/]")
    else:
        note = "" if rl.get("players") else " (no NHL regular-season games that day)"
        console.print(f"NHL results {rl['day']}: {rl.get('players', 0)} players{note}")
    for g in summary.get("grade") or []:
        _print_grade(g)
    console.print(f"[dim]{summary['seconds']}s[/]")


@harness_app.command("ledger")
def ledger_cmd(league: str = typer.Option("all", "--league", "-l", help="espn | fantrax | all"),
               kind: Optional[str] = typer.Option(None, "--kind", help="waiver | lineup | trade | injury | "
                                                                       "sell_high | buy_low"),
               origin: Optional[str] = typer.Option(None, "--origin",
                                                    help="Episode status (open | followed | partial | proposed | "
                                                         "expired) or user_only (my moves without a rec)."),
               since: Optional[str] = typer.Option(None, "--since", help="Only episodes / moves since YYYY-MM-DD."),
               limit: int = typer.Option(30, "--limit", "-n"),
               json_out: bool = typer.Option(False, "--json")) -> None:
    """Recent recommendation episodes with their match status (and my user-only moves)."""
    settings = _settings()
    leagues = _leagues(league)
    since_s = (_parse_day(since) or date(1900, 1, 1)).isoformat()
    ph = ",".join("?" * len(leagues))
    ledger = _ledger(settings)
    try:
        eps: list[dict[str, Any]] = []
        if origin != "user_only":
            sql = (f"SELECT league, kind, status, first_seen, last_seen, n_days, predicted_gain, gain_units, "
                   f"horizon_days, strength, acted_on, window_end, title FROM rec_episodes "
                   f"WHERE league IN ({ph}) AND last_seen >= ?")
            params: list[Any] = [*leagues, since_s]
            if kind:
                sql += " AND kind = ?"
                params.append(kind)
            if origin:
                sql += " AND status = ?"
                params.append(origin)
            eps = ledger.query(sql + " ORDER BY last_seen DESC, first_seen DESC LIMIT ?", [*params, limit])
        moves: list[dict[str, Any]] = []
        if origin in (None, "user_only"):
            sql = f"SELECT league, day, kind, adds_json, drops_json FROM decisions WHERE origin='user_only' " \
                  f"AND league IN ({ph}) AND day >= ?"
            params = [*leagues, since_s]
            if kind:
                sql += " AND kind = ?"
                params.append(kind)
            moves = ledger.query(sql + " ORDER BY day DESC LIMIT ?", [*params, limit])
            names = {r["cid"]: r["player_name"] for r in ledger.query(
                "SELECT DISTINCT cid, player_name FROM transactions WHERE is_me=1")}
            for m in moves:
                m["adds"] = [names.get(c) or c for c in json.loads(m.pop("adds_json") or "[]")]
                m["drops"] = [names.get(c) or c for c in json.loads(m.pop("drops_json") or "[]")]
    finally:
        ledger.close()
    if json_out:
        typer.echo(json.dumps({"episodes": eps, "user_only": moves}, indent=2, default=str))
        return
    if origin != "user_only":
        if not eps:
            console.print("No episodes yet: run `fm harness daily`.")
        else:
            t = Table(title="Recommendation episodes (newest first)")
            for c, j in (("League", "left"), ("Kind", "left"), ("Status", "left"), ("Seen", "left"),
                         ("Days", "right"), ("Gain", "right"), ("Str", "right"), ("Recommendation", "left")):
                t.add_column(c, justify=j)
            for e in eps:
                seen = e["first_seen"] if e["first_seen"] == e["last_seen"] else f"{e['first_seen']}..{e['last_seen'][5:]}"
                g = e["predicted_gain"]
                gain = "-" if g is None else f"{g:+.2f} {e['gain_units'] or ''}".strip()
                t.add_row(e["league"], e["kind"], e["status"], seen, str(e["n_days"]), gain,
                          "-" if e["strength"] is None else f"{e['strength']:.1f}", escape(e["title"] or ""))
            console.print(t)
    if moves:
        t = Table(title="My moves without a recommendation (user_only)")
        for c in ("League", "Day", "Kind", "Added", "Dropped"):
            t.add_column(c)
        for m in moves:
            t.add_row(m["league"], m["day"], m["kind"] or "-", escape(", ".join(m["adds"]) or "-"),
                      escape(", ".join(m["drops"]) or "-"))
        console.print(t)


@harness_app.command("rebuild")
def rebuild_cmd(everything: bool = typer.Option(False, "--all",
                                                help="Also drop pulled data (transactions, lineups, NHL results) "
                                                     "that the archive cannot restore.")) -> None:
    """Drop and rebuild the archive-derived tables from data/archive, then re-match."""
    from .harness.ingest import ingest_archive, record_run
    from .harness.ledger import TABLES
    from .harness.match import match_episodes

    settings = _settings()
    ledger = _ledger(settings)
    try:
        keep = set() if everything else {"transactions", "lineup_days", "realized_daily", "realized_pulls", "runs"}
        for t in TABLES:
            if t not in keep:
                ledger.execute(f"DELETE FROM {t}")
        ledger.commit()
        ing = ingest_archive(ledger, settings.fm_data_dir, force=True)
        lines = []
        for lg in sorted({r["league"] for r in ledger.query("SELECT DISTINCT league FROM rec_episodes")}):
            lines.append(match_episodes(ledger, lg).line())
        record_run(ledger, "rebuild", None, date.today(), {"ingest": ing, "match": lines})
    finally:
        ledger.close()
    console.print(f"Rebuilt from {ing['files_ingested']} archive file(s): {ing['projections']} projection rows, "
                  f"{ing['recs']} recs, {ing['episodes']} episodes."
                  + ("" if everything else " Pulled transactions / lineups / NHL results kept."))
    for ln in lines:
        console.print(escape(ln))
    for e in ing["errors"]:
        console.print(f"  [yellow]{escape(e)}[/]")


def _print_grade(g: dict[str, Any]) -> None:
    if g.get("error"):
        console.print(f"[red]{g['league']}: grading failed: {escape(g['error'])}[/]")
        return
    console.print(escape(g.get("outcomes_line") or ""))
    w = g.get("week") or {}
    if w.get("note"):
        console.print(f"{g['league']}: {escape(w['note'])}")
        return
    t = w.get("trust") or {}
    console.print(f"{g['league']}: bar for week {w.get('week')}: {w.get('rows', 0)} metric rows "
                  f"({t.get('reliable', 0)} reliable, {t.get('provisional', 0)} provisional, "
                  f"{t.get('hidden', 0)} hidden); {w.get('matured_28d', 0)} matured 28-day and "
                  f"{w.get('matured_7d', 0)} 7-day projection snapshot(s) of {w.get('snapshots', 0)}, "
                  f"{w.get('outcomes_used', 0)} matured outcome(s)")


@harness_app.command("grade")
def grade_cmd(week: Optional[str] = typer.Option(None, "--week",
                                                 help="Week to snapshot (a Monday, YYYY-MM-DD; another day means "
                                                      "its week). Default: the most recent Monday."),
              league: str = typer.Option("all", "--league", "-l", help="espn | fantrax | all"),
              json_out: bool = typer.Option(False, "--json")) -> None:
    """Grade every matured outcome, then compute that week's bar (metric snapshots + trust labels)."""
    from .harness.ingest import record_run

    settings = _settings()
    today = date.today()
    wk = last_monday(_parse_day(week) or today)
    ledger = _ledger(settings)
    try:
        results = _grade_leagues(ledger, _leagues(league), wk, today)
        record_run(ledger, "grade", league, today, {"week": wk.isoformat(), "leagues": results},
                   "partial" if any(r.get("error") for r in results) else "ok")
    finally:
        ledger.close()
    if json_out:
        typer.echo(json.dumps({"week": wk.isoformat(), "leagues": results}, indent=2, default=str))
        return
    for r in results:
        _print_grade(r)


# --------------------------------------------------------------------------- status: the bar

def _f(v: Any, nd: int = 2, pct: bool = False) -> str:
    if v is None:
        return "-"
    return f"{v * 100:.0f}%" if pct else f"{v:.{nd}f}"


TRUST_STYLE = {"reliable": "green", "provisional": "yellow", "hidden": "dim", "cases": "cyan"}


def _trust(t: str) -> str:
    return f"[{TRUST_STYLE.get(t, 'white')}]{t}[/]"


def _print_projection(proj: list[dict[str, Any]]) -> None:
    by = {(r["metric"], r["pool"]): r for r in proj}
    t = Table(title="Projection accuracy (fpg vs next 28 days, >= 8 GP; proj_week vs next 7 days)")
    for c in ("Pool", "MAE", "Spearman", "Skill vs to-date [95% CI]", "N", "Trust", "Week MAE",
              "rate / avail", "N wk", "Trust wk"):
        t.add_column(c, justify="left" if c in ("Pool", "Trust", "Trust wk") else "right")
    for pool in ("F", "D", "G"):
        m, sk, wk = by.get(("proj_fpg_mae", pool)), by.get(("proj_fpg_skill", pool)), by.get(("proj_week_mae", pool))
        if not m:
            continue
        d, dw = m["detail"], (wk or {}).get("detail") or {}
        ci = (f" [{_f(sk['ci_lo'], pct=True)}, {_f(sk['ci_hi'], pct=True)}]"
              if sk and sk.get("ci_lo") is not None else "")
        t.add_row(pool, _f(m["value"]), _f(d.get("spearman")),
                  (_f(sk["value"], pct=True) + ci) if sk else "-", str(m["n"]), _trust(m["trust"]),
                  _f((wk or {}).get("value")), f"{_f(dw.get('rate_mae'))} / {_f(dw.get('avail_mae'))}",
                  str((wk or {}).get("n", 0)), _trust((wk or {}).get("trust", "hidden")))
    console.print(t)


def _print_hits(hits: list[dict[str, Any]]) -> None:
    t = Table(title="Recommendation hit rates (complete windows)")
    for c in ("Kind:origin", "Hit rate", "95% CI", "Mean realized", "N", "Trust"):
        t.add_column(c, justify="left" if c in ("Kind:origin", "Trust") else "right")
    for r in hits:
        d = r["detail"]
        ci = f"{_f(r['ci_lo'], pct=True)}-{_f(r['ci_hi'], pct=True)}" if r["ci_lo"] is not None else "-"
        t.add_row(r["pool"], _f(r["value"], pct=True), ci, f"{_f(d.get('mean_gain'))} {d.get('units', '')}",
                  str(r["n"]), _trust(r["trust"]))
    console.print(t)


def _print_calibration(cal: dict[str, Any] | None) -> None:
    if not cal:
        return
    if not cal["detail"].get("bins"):
        console.print(f"Calibration: {cal['n']} graded rec(s) with a predicted gain ({_trust(cal['trust'])}).")
        return
    t = Table(title=f"Calibration: predicted vs realized pts ({cal['detail'].get('binning')}, n={cal['n']}, "
                    f"{cal['trust']})")
    for c in ("Bin", "Predicted range", "Mean predicted", "Mean realized", "Hit rate", "N"):
        t.add_column(c, justify="right")
    for b in cal["detail"]["bins"]:
        t.add_row(str(b["bin"]), f"{b['pred_lo']:.1f} .. {b['pred_hi']:.1f}", f"{b['mean_pred']:.2f}",
                  f"{b['mean_real']:.2f}", _f(b["hit_rate"], pct=True), str(b["n"]))
    console.print(t)


def _print_counterfactual(cf: dict[str, Any] | None) -> None:
    if not cf:
        return
    d = cf["detail"]
    if cf["n"]:
        console.print(f"Your moves vs the model's: you {_f(d.get('my_mean'))} pts vs the model's pick "
                      f"{_f(d.get('model_mean'))} pts on {cf['n']} paired move(s); model better "
                      f"{_f(d.get('model_better'), pct=True)}, model agreed with {_f(d.get('model_agreed'), pct=True)}"
                      f" ({_trust(cf['trust'])})")
    else:
        console.print(f"Your moves vs the model's: {d.get('moves', 0)} graded user-only waiver move(s), none "
                      f"paired with a model pick yet ({_trust(cf['trust'])}).")


def _print_bar(rep: dict[str, Any]) -> None:
    pi = rep.get("params") or {}
    console.print(f"Params: {pi.get('version', '-')} (hash {(pi.get('hash') or '-')[:10]}, "
                  f"{escape(str(pi.get('source', '-')))}; {pi.get('versions', 0)} fitted version(s))")
    for lg, L in (rep.get("leagues") or {}).items():
        console.rule(f"{lg}: " + (f"bar as of week {L['week']}" if L.get("graded") else "not graded yet"))
        if not L.get("graded"):
            console.print("[dim]Run `fm harness grade` (it also runs with `fm harness daily` on Mondays).[/]")
        if L.get("projection"):
            _print_projection(L["projection"])
        if L.get("hit_rates"):
            _print_hits(L["hit_rates"])
        _print_calibration(L.get("calibration"))
        _print_counterfactual(L.get("counterfactual"))
        trades = L.get("trades") or []
        console.print(f"Trades (case list, never aggregated): {len(trades) or 'none acted on yet'}")
        for c in trades[:10]:
            console.print(f"  {c.get('day')} {escape(str(c.get('title') or '-'))} [{c['origin']}, {c['window']}]: "
                          f"{_f(c.get('realized_gain'))} pts ({c.get('label')})")
        oc = L.get("outcomes") or {}
        console.print(f"Outcomes: {oc.get('n', 0)} rows, {oc.get('complete', 0)} complete, "
                      f"{oc.get('ungradable', 0)} without a realized gain, {oc.get('unmatched', 0)} with unmatched "
                      "players")
        nj = L.get("not_judgeable") or []
        if nj:
            items = [f"{x['metric']} {x['pool']} (n={x['n']}, need {x['need'] if x['need'] is not None else '?'} more)"
                     for x in nj]
            console.print("[dim]Not judgeable yet: " + "; ".join(items) + "[/]")
        hl = L.get("headline")
        console.print(f"Digest line: {escape(hl)}" if hl else "[dim]Digest line: hidden (nothing trustworthy yet)[/]")


@harness_app.command("status")
def status_cmd(json_out: bool = typer.Option(False, "--json"),
               league: str = typer.Option("all", "--league", "-l", help="espn | fantrax | all")) -> None:
    """The bar (projection accuracy, hit rates, calibration, trust labels) and what the ledger holds."""
    from .harness.metrics import known_leagues, status_report

    settings = _settings()
    ledger = _ledger(settings)
    try:
        wanted = _leagues(league)
        bar = status_report(ledger, [lg for lg in known_leagues(ledger) if lg in wanted] or wanted)
        eps = ledger.query("SELECT league, kind, status, COUNT(*) n FROM rec_episodes GROUP BY 1, 2, 3 ORDER BY 1, 2, 3")
        tx = ledger.query("SELECT league, is_me, COUNT(*) n FROM transactions GROUP BY 1, 2")
        dec = ledger.query("SELECT league, origin, COUNT(*) n FROM decisions GROUP BY 1, 2")
        proj = ledger.query("SELECT league, COUNT(DISTINCT as_of) days, COUNT(*) rows, "
                            "SUM(inputs_json IS NOT NULL) with_inputs FROM projections GROUP BY 1")
        lineups = ledger.query("SELECT league, COUNT(DISTINCT day) days, COUNT(*) rows FROM lineup_days GROUP BY 1")
        pulls = ledger.query("SELECT COUNT(*) pulled, SUM(n_players > 0) with_games, MAX(game_date) last, "
                             "SUM(n_players) player_rows FROM realized_pulls")[0]
        last_run = ledger.query("SELECT command, league, started_at, status FROM runs ORDER BY run_id DESC LIMIT 1")
        path = str(ledger.path)
    finally:
        ledger.close()
    payload = {"db": path, "bar": bar, "episodes": eps, "transactions": tx, "decisions": dec, "projections": proj,
               "lineups": lineups, "realized": pulls, "last_run": last_run[0] if last_run else None}
    if json_out:
        typer.echo(json.dumps(payload, indent=2, default=str))
        return
    console.print(f"[dim]{escape(path)}[/]")
    _print_bar(bar)
    console.rule("Ledger")
    if eps:
        t = Table(title="Recommendation episodes")
        for c, j in (("League", "left"), ("Kind", "left"), ("Status", "left"), ("N", "right")):
            t.add_column(c, justify=j)
        for e in eps:
            t.add_row(e["league"], e["kind"], e["status"], str(e["n"]))
        console.print(t)
    else:
        console.print("No episodes yet: run `fm harness daily`.")
    for p in proj:
        console.print(f"{p['league']}: projections {p['rows']} rows over {p['days']} day(s), "
                      f"{p['with_inputs'] or 0} with v2 inputs")
    for x in tx:
        who = "mine" if x["is_me"] else "other teams"
        console.print(f"{x['league']}: {x['n']} transaction rows ({who})")
    for d in dec:
        console.print(f"{d['league']}: {d['n']} decisions {d['origin']}")
    for ln in lineups:
        console.print(f"{ln['league']}: lineups {ln['rows']} rows over {ln['days']} day(s)")
    console.print(f"NHL results: {pulls['pulled'] or 0} day(s) pulled, {pulls['with_games'] or 0} with games, "
                  f"{pulls['player_rows'] or 0} player-games (last {pulls['last'] or '-'})")
    if last_run:
        r = last_run[0]
        console.print(f"Last run: {r['command']} {r['league'] or ''} at {r['started_at']} ({r['status']})")
