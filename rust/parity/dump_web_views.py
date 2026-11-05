"""Python goldens for W9-web-views: every page's view-model dict (web/views.py), the chart SVGs
(web/charts.py) and the /health chart specs (web/health_view.py), from the parity snapshot with a
pinned clock.

    C:/code/fantasy_manager/.venv/Scripts/python rust/parity/dump_web_views.py <fixtures dir> <out dir> [--leagues espn,fantrax]

Writes <out dir>/<league>.json (a dict page name -> view model) and <out dir>/charts.json.
``<fixtures dir>`` is the full parity fixture set (snapshot.json, values.json, dynasty.json,
engines/advise_merged.json per league and the shared ``_datadir`` with harness.db).

The view models contain engine objects (Player, Matchup, WeekPlan, ...). They are dumped with
``_pj``: pydantic ``model_dump(mode="json")`` / dataclass fields, plus the ``@property`` values the
templates read on a few classes (PROPS below), so the JSON is the template context itself. The
Rust ``fm_web::views`` produces the same JSON. Dates are ISO strings, tuples lists.

The provider (needed by the playoff calendar, the matchup and the trade-block wants) is the real
one, loaded from the offline HTTP cache (cache.db) with the clock pinned to the snapshot's.
"""
from __future__ import annotations

import argparse
import copy
import dataclasses
import datetime as dt
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from types import SimpleNamespace
from typing import Any

WORKTREE = Path(__file__).resolve().parents[2]
REPO = Path(os.environ.get("FM_REPO", r"C:\code\fantasy_manager"))

# class name -> @property names the templates read (added to the dumped dict)
PROPS = {
    "Matchup": ("my_projected_total", "their_projected_total", "margin"),
    "PositionGap": ("gap",),
    "PlayoffWeeks": (), "PlayoffPlan": (), "PeriodCalendar": (),
    "Period": ("days", "label"),
    "WeekView": ("label",),
    "WeekPlan": ("lineup_gain", "moves_gain"),
    "PlanSince": ("empty",),
    "TeamOutlook": ("future_avg", "mine"),
    "Outlook": (), "Asset": (),
}


def _clean(v: Any) -> Any:
    """Non-finite floats (inside model_dump output) -> None, like serde_json."""
    if isinstance(v, float):
        return v if v == v and v not in (float("inf"), float("-inf")) else None
    if isinstance(v, dict):
        return {k: _clean(x) for k, x in v.items()}
    if isinstance(v, list):
        return [_clean(x) for x in v]
    return v


def _pj(x: Any) -> Any:
    return _clean(_pj0(x))


def _pj0(x: Any) -> Any:
    from dump_fixtures import jsonable
    name = type(x).__name__
    if name in PROPS and (hasattr(x, "model_dump") or dataclasses.is_dataclass(x)):
        if hasattr(x, "model_dump"):
            d = {k: _pj(getattr(x, k)) for k in type(x).model_fields}
        else:
            d = {f.name: _pj(getattr(x, f.name)) for f in dataclasses.fields(x)}
        for p in PROPS[name]:
            d[p] = _pj(getattr(x, p))
        return d
    if isinstance(x, dict):
        return {(k if isinstance(k, str) else k.isoformat() if isinstance(k, dt.date) else str(k)): _pj(v)
                for k, v in x.items()}
    if isinstance(x, (list, tuple)):
        return [_pj(v) for v in x]
    return jsonable(x)


def main() -> None:
    if os.environ.get("PYTHONHASHSEED") is None:
        env = dict(os.environ, PYTHONHASHSEED="0")
        raise SystemExit(subprocess.call([sys.executable, *sys.argv], env=env))
    ap = argparse.ArgumentParser()
    ap.add_argument("fixtures")
    ap.add_argument("out")
    ap.add_argument("--leagues", default="espn,fantrax")
    args = ap.parse_args()
    fixtures, out = Path(args.fixtures), Path(args.out)
    out.mkdir(parents=True, exist_ok=True)

    sys.path.insert(0, str(WORKTREE))
    sys.path.insert(0, str(WORKTREE / "rust" / "parity"))
    os.chdir(REPO)
    tmp = Path(tempfile.mkdtemp(prefix="fm_webviews_"))
    shutil.copytree(fixtures / "_datadir", tmp / "data")
    os.environ["FM_DATA_DIR"] = str(tmp / "data")
    os.environ["FM_NEWS_ROLES_LLM"] = "0"
    os.environ["FM_OFFLINE"] = "1"
    os.environ.setdefault("FANTRAX_KEEPERS", "3")
    (out / "datadir_note.txt").write_text(f"python data dir copy: {tmp / 'data'}\n")

    import dump_fixtures as dfx

    dump_charts(out)
    for league in [x.strip() for x in args.leagues.split(",") if x.strip()]:
        dump_league(league, fixtures, out, tmp / "data", dfx)
    shutil.rmtree(tmp, ignore_errors=True)


# --------------------------------------------------------------------------- charts

def synthetic_series() -> dict[str, Any]:
    nan = None
    x = ["2026-10-05", "2026-10-12", "2026-10-19", "2026-10-26", "2026-11-02"]
    pool = lambda lo: {  # noqa: E731
        "x": x, "trust": "ok", "n": 120, "need": 100, "value": 0.1234 + lo,
        "lines": {"fm": [0.5 + lo, 0.48 + lo, nan, 0.45 + lo, 0.44 + lo],
                  "to_date": [0.55 + lo, 0.5 + lo, 0.49 + lo, 0.5 + lo, 0.47 + lo],
                  "preseason": [0.6, 0.58, 0.57, 0.56, 0.55],
                  "provider": [nan, nan, nan, nan, nan],
                  "last_season": [0.65, 0.64, 0.6, 0.61, 0.62]},
        "n_by_week": [20, 40, 60, 80, 120]}
    skill = {"x": x, "trust": "provisional", "n": 50, "need": 100, "value": 0.087,
             "lines": {"fm": [0.05, -0.02, 0.0, 0.08, 0.087], "to_date": [0.0, 0.0, 0.0, 0.0, 0.0]},
             "n_by_week": [10, 20, 30, 40, 50]}
    hit = lambda trust, n, rate: {  # noqa: E731
        "x": x[:n], "trust": trust, "n": 33, "need": 30, "value": rate[-1] if rate else None,
        "lines": {"rate": rate}, "band": {"lo": [max(0.0, r - 0.2) for r in rate], "hi": [min(1.0, r + 0.2) for r in rate]},
        "n_by_week": [5 * (i + 1) for i in range(n)]}
    return {"mae": {"F": pool(0.0), "D": pool(0.2), "G": {"x": [], "trust": "hidden", "n": 0, "need": 100,
                                                          "value": None, "lines": {}}},
            "skill": {"F": skill, "D": skill, "G": {"x": x, "trust": "hidden", "n": 2, "need": 100, "lines": {}}},
            "hit": {"waiver": hit("ok", 5, [0.5, 0.55, 0.6, 0.58, 0.62]),
                    "lineup": hit("ok", 1, [0.7]),
                    "sell_high": hit("hidden", 0, [])}}


def dump_charts(out: Path) -> None:
    from fantasy_manager.web import charts, health_view
    from dump_fixtures import write_json

    cs = charts
    res: dict[str, Any] = {}
    x = ["2026-10-05", "2026-10-12", "2026-10-19", "2026-10-26"]
    L = cs.Line
    lines = [L("fm", "fm model", [0.5, 0.48, None, 0.45], "main"), L("a", "Base <A>", [0.55, 0.5, 0.49, 0.5], "base", "5 3"),
             L("b", "Base B", [None, None, None, None], "base", "1.5 2.5")]
    res["line_chart_basic"] = str(cs.line_chart("My Chart #1", x, lines, title="T & \"q\"", desc="d <x>"))
    res["line_chart_band_zero"] = str(cs.line_chart(
        "z", x, [L("rate", "Hit rate", [0.5, 0.6, 0.0, -0.2], "main")], title="t", desc="d",
        band=([0.3, 0.4, -0.1, -0.4], [0.7, 0.8, 0.1, 0.0]), y_fmt=lambda v: f"{v * 100:.0f}%", zero_line=True,
        y_domain=None))
    res["line_chart_domain"] = str(cs.line_chart("d", ["wk1"], [L("rate", "Hit", [0.5], "main")], title="t", desc="d",
                                                 band=([0.4], [0.9]), y_domain=(0.0, 1.0)))
    res["line_chart_empty"] = str(cs.line_chart("", [], [L("rate", "Hit", [], "main")], title="t", desc="d"))
    res["line_chart_flat"] = str(cs.line_chart("flat", x, [L("k", "Flat", [2.0, 2.0, 2.0, 2.0], "main")], title="t",
                                               desc="d"))
    res["legend"] = str(cs.legend(lines, "95% CI (Wilson)"))
    res["domain"] = [list(cs.domain([1.0, 3.0, None])), list(cs.domain([2.0, 2.0])), list(cs.domain([])),
                     list(cs.domain([-1.0, 4.0], include_zero=True)), list(cs.domain([0.0, 0.0]))]
    res["x_positions"] = [cs.x_positions(0), cs.x_positions(1), cs.x_positions(4)]
    res["scale_points"] = [[list(p) if p else None for p in cs.scale_points([1.0, None, 3.0], 0.0, 4.0)]]
    res["sparkline_path"] = cs.sparkline_path([(1.0, 2.0), None, (3.5, 4.25), (5.0, 6.0), None])
    res["band_path"] = [cs.band_path([(1.0, 2.0), (3.0, 4.0), None, (7.0, 8.0)], [(1.0, 1.0), (3.0, 3.0), (5.0, 5.0), (7.0, 7.0)])]
    series = synthetic_series()
    hc = health_view.health_charts(series, "espn")
    res["health_charts"] = _pj(json.loads(json.dumps(hc, default=str)))
    res["health_charts_none"] = _pj(json.loads(json.dumps(health_view.health_charts(None, "fantrax"), default=str)))
    res["series"] = series
    res["refit_rows"] = health_view.refit_rows({"rows": [
        {"knob": "k1", "current": 0.123456789, "candidate": 1234567.0, "holdout_before": 0.1, "holdout_after": 0.09,
         "hist_before": None, "hist_after": 0.2},
        {"knob": "k2", "current": 1e-05, "candidate": 2.5, "holdout_before": None, "holdout_after": None}]})
    res["fmt"] = {"pct": [health_view.pct(None), health_view.pct(0.456), health_view.pct(1)],
                  "num": [health_view.num(None), health_view.num(1.23456, 3), health_view.num(2)],
                  "signed_pct": [health_view.signed_pct(None), health_view.signed_pct(0.456),
                                 health_view.signed_pct(-0.004)]}
    write_json(out / "charts.json", res)


# --------------------------------------------------------------------------- league pages

def restore_registries(extras: dict[str, Any]) -> None:
    """Refill the enrichment registries from the snapshot (pick / rookie valuation reads them)."""
    from fantasy_manager.providers import lines_enrich, news_roles, nhl, preseason_enrich, rookie_enrich, xg_enrich
    from fantasy_manager.valuation.regression import GoalieGsax

    for m in (preseason_enrich, rookie_enrich, xg_enrich):
        m.clear_registry()
    lines_enrich._TEAM_META.clear()
    preseason_enrich.register([preseason_enrich.PreseasonLine(**d) for d in extras["preseason_lines"]])
    for nid, rows in extras["rookie_history"].items():
        rookie_enrich.register_history(int(nid), [nhl.LeagueSeason.model_validate(r) for r in rows])
    for cid, sigs in extras["rookie_signals"].items():
        rookie_enrich.register_signals(cid, [news_roles.RoleSignal.model_validate(s) for s in sigs])
    for g in extras["gsax"]:
        g = dict(g)
        sy = g.pop("season_start_year")
        g["seasons"] = tuple(g["seasons"])
        xg_enrich.register_gsax(sy, GoalieGsax(**g))
    for k, v in extras["xg_norms"].items():
        xg_enrich._NORMS[int(k)] = v
    for k, v in extras["line_team_meta"].items():
        lines_enrich._TEAM_META[dt.date.fromisoformat(k)] = v


def dump_league(league: str, fixtures: Path, out: Path, data_dir: Path, dfx: Any) -> None:
    from fantasy_manager import cli
    from fantasy_manager.cache import HttpCache
    from fantasy_manager.models import LeagueContext, Recommendation
    from fantasy_manager.valuation.dynasty import DynastyValue
    from fantasy_manager.valuation.valuate import PlayerValue
    from fantasy_manager.valuation.rookie import RookieEstimate
    from fantasy_manager.web import views
    from fantasy_manager.config import get_settings

    snap = json.loads((fixtures / league / "snapshot.json").read_text(encoding="utf-8"))
    now = dt.datetime.fromisoformat(snap["now"])
    dfx.freeze_clock(now)
    for k, v in (snap.get("env") or {}).items():
        if v is None:
            os.environ.pop(k, None)
        elif k not in ("FM_OFFLINE",):
            os.environ[k] = v
    os.environ["FM_DATA_DIR"] = str(data_dir)
    os.environ["FM_NEWS_ROLES_LLM"] = "0"
    os.environ["FM_OFFLINE"] = "1"
    get_settings.cache_clear()
    restore_registries(snap["extras"])
    # the run that wrote the snapshot had a harness params version active (not in the data dir copy)
    from fantasy_manager.valuation import params as vparams
    vparams.reload()
    with vparams._lock:
        vparams._merged = snap["params"]["merged"]
        vparams._active = snap["params"]["active"]
        vparams._memo.clear()
    ctx = LeagueContext.model_validate(snap["ctx"])
    players = {p.cid: p for p in ctx.all_players()}
    vals = {}
    for cid, v in json.loads((fixtures / league / "values.json").read_text(encoding="utf-8")).items():
        v = dict(v)
        v.pop("cid", None)
        v["rookie"] = RookieEstimate.model_validate(v["rookie"]) if v.get("rookie") else None
        vals[cid] = PlayerValue(player=players[cid], **v)
    raw = json.loads((fixtures / league / "dynasty.json").read_text(encoding="utf-8"))
    dyn = None
    if raw is not None:
        dyn = {}
        for cid, v in raw.items():
            v = dict(v)
            v.pop("cid", None)
            dyn[cid] = DynastyValue(player=players[cid], **v)
    recs = [Recommendation.model_validate(r)
            for r in json.loads((fixtures / league / "engines" / "advise_merged.json").read_text(encoding="utf-8"))]

    from fantasy_manager.analysis import matchup as _mu
    if not hasattr(_mu, "_orig_current_matchup"):
        _mu._orig_current_matchup = _mu.current_matchup
        _mu.current_matchup = lambda *a, **k: _mu._orig_current_matchup(*a, **{**k, "seed": 7})
    cache = HttpCache(REPO / "data", offline=True)
    cli._cache = lambda: cache
    _raw_ctx, provider = cli._load_context(league, cache)

    # recorded status changes for one of my players (status_history)
    mine = ctx.my_team
    first = next(p for p in mine.players)
    t0 = int(now.timestamp()) - 3 * 86400
    status_log = {first.cid: [SimpleNamespace(seen_at=t0, status="healthy", note=None),
                              SimpleNamespace(seen_at=t0 + 86400, status="dtd", note="upper body")]}
    res = SimpleNamespace(ctx=ctx, values=vals, dynasty=dyn, recs=recs, warnings=[], memo={}, status_log=status_log)
    pages: dict[str, Any] = {}

    def page(name: str, fn) -> None:  # noqa: ANN001
        try:
            pages[name] = _pj(fn())
        except Exception as e:  # noqa: BLE001
            pages[name] = {"__error__": f"{type(e).__name__}: {e}"}
            print(f"{league}: page {name} failed: {type(e).__name__}: {e}", flush=True)

    page("week_start", lambda: list(views.week_start(ctx)))
    page("week_panel", lambda: views.week_panel(res))
    page("standings", lambda: views.standings(res))
    page("free_agents", lambda: views.free_agents(res))
    page("free_agents_D_games", lambda: views.free_agents(res, limit=6, pos="D", sort="games"))
    page("free_agents_F_fpgweek", lambda: views.free_agents(res, limit=6, pos="F", sort="fpg_week"))
    page("free_agents_q", lambda: views.free_agents(res, q="an"))
    page("league_injuries", lambda: views.league_injuries(res))
    page("roster_rows", lambda: [views.player_row(res, s.player, slot=s.slot) for s in mine.slots if s.player])
    page("draft_picks", lambda: views.draft_picks(res, mine))
    page("credits", lambda: views.credits(ctx))
    page("limits_text", lambda: views.limits_text(ctx))
    page("track_strip", lambda: views.track_strip(league))
    page("outlook", lambda: views.outlook_section(res, provider))
    page("matchup_strip", lambda: views.matchup_strip(res, provider))
    page("matchup", lambda: views.matchup_page(res, provider))
    page("plan", lambda: views.plan_page(res, provider))
    page("schedule", lambda: views.schedule_page(res, None, provider))
    page("schedule_week", lambda: views.schedule_page(res, "2026-11-18", provider, limit=4))
    page("claims", lambda: views.claims_section(res, provider))
    page("exploits", lambda: views.exploits(res))
    page("need_trades", lambda: views.need_trades(res))
    page("warnings", lambda: list(res.warnings))

    def moves():
        rows = []
        for r in res.recs:
            rows.append({"title": r.title, "kind": r.kind, "compare": views.compare(res, r),
                         "gain": views.gain_info(r, res), "gain_nores": views.gain_info(r),
                         "strength": views.strength_info(r, res.recs), "strength_alone": views.strength_info(r),
                         "market": views.market_info(r), "track": views.track_info(r, league),
                         "trade": views.trade_info(r),
                         "counterparty": views.counterparty(res, r) if r.kind == "trade" else None,
                         "involves": {pos: views.involves_position(r, pos) for pos in views.POSITION_FILTERS}})
        return rows
    page("moves", moves)

    p0 = first
    def player_page():
        weights = ctx.scoring.weights if ctx.scoring.kind == "points" else None
        cols, rows = views.split_table(p0, weights)
        start, _, pre = views.week_start(ctx)
        return {"cid": p0.cid, "cols": cols, "rows": rows, "schedule": views.team_games(ctx, p0.team, start, 14),
                "history": views.status_history(res, p0.cid), "history_none": views.status_history(res, "nope"),
                "breakdown": views.dynasty_breakdown((dyn or {}).get(p0.cid)),
                "breakdown_none": views.dynasty_breakdown(None), "preseason": pre,
                "row": views.player_row(res, p0, slot="C"), "xg": views.xg_luck(p0),
                "age": views.player_age(p0, ctx.as_of, dyn), "badges": views.unit_badges(p0)}
    page("player", player_page)

    # trade evaluator
    other = next((t for t in ctx.teams if not t.owner_is_me), None)

    def best(team):  # noqa: ANN001
        ps = [p for p in team.players if p.cid in vals]
        return max(ps, key=lambda p: (vals[p.cid].fpg_season, p.cid), default=None)
    give, get = best(mine), best(other)
    page("trade_eval_empty", lambda: views.trade_eval_page(res, None, [], []))
    page("trade_eval_offer", lambda: views.trade_eval_page(res, None, [give.cid], [get.cid], provider=provider))
    page("trade_eval_get_only", lambda: views.trade_eval_page(res, "no-such-team", [], [get.cid], provider=provider))
    page("trade_eval_switch", lambda: views.trade_eval_page(res, other.team_id, [give.cid], [get.cid], provider=provider,
                                                             evaluate=False))
    try:
        from fantasy_manager.recommend.trades import pick_assets
        pa = pick_assets(ctx, vals, dyn)
        mp = pa.get(mine.team_id, [])
        tp = pa.get(other.team_id, [])
        if mp or tp:
            page("trade_eval_picks", lambda: views.trade_eval_page(
                res, other.team_id, [give.cid], [get.cid], [mp[0].key] if mp else [], [tp[0].key] if tp else [],
                provider=provider))
    except Exception as e:  # noqa: BLE001
        print("pick_assets failed", e)
    page("trade_eval_error", lambda: views.trade_eval_page(res, other.team_id, [], [], ["pick:2099:9:zzz"], [],
                                                            provider=provider))
    page("team_color", lambda: {a: views.team_color(a) for a in ("TOR", "la", "NJ", "ZZZ", None, " utah ")})
    page("games_text", lambda: [views.games_text(views.team_games(ctx, "TOR", ctx.as_of, 7)),
                                views.games_text([])])
    page("horizon_label", lambda: [views.horizon_label(x) for x in (None, 3, 7, 14, 120, 200)])

    dfx.write_json(out / f"{league}.json", pages)
    print(f"{league}: {len(pages)} pages -> {out / (league + '.json')}", flush=True)


if __name__ == "__main__":
    main()
