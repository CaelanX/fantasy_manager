"""Python goldens for W10-web-app: every dashboard page's HTML (and the JSON APIs) rendered by the
FastAPI app (``web.app.create_app``) through TestClient, from the parity snapshot with a pinned
clock and a seeded matchup simulation.

    C:/code/fantasy_manager/.venv/Scripts/python parity/dump_web_html.py <fixtures dir> <out dir> [--leagues espn,fantrax]

Writes <out dir>/<league>.json: {"pages": {name: {"url", "status", "content_type", "body"}},
"meta": {...}}. The Rust test (crates/fm-web/tests/app.rs) requests the same URLs from its router
with the same LoadResult and compares the bodies (timestamps / cache-busting tokens normalized).

The LoadResult is built like dump_web_views.py's (snapshot context, values.json, dynasty.json,
engines/advise_merged.json, a synthetic two-entry status history for my first player) plus the
news matched from the data dir's 30-day news store (no network), loaded_at = the pinned clock and
the effective dynasty mode. The provider is the real one from the offline HTTP cache (cache.db).
"""
from __future__ import annotations

import argparse
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

WORKTREE = Path(os.environ.get("FM_LEGACY_ROOT") or Path(__file__).resolve().parents[2] / "fm-legacy")  # legacy-python checkout
REPO = Path(os.environ.get("FM_REPO", r"C:\code\fantasy_manager"))


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
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    os.chdir(REPO)
    tmp = Path(tempfile.mkdtemp(prefix="fm_webhtml_"))
    shutil.copytree(fixtures / "_datadir", tmp / "data")
    os.environ["FM_DATA_DIR"] = str(tmp / "data")
    os.environ["FM_NEWS_ROLES_LLM"] = "0"
    os.environ["FM_OFFLINE"] = "1"
    os.environ.setdefault("FANTRAX_KEEPERS", "3")
    for k in ("FM_WEB_PASSWORD", "OPENROUTER_API_KEY"):
        os.environ.pop(k, None)

    import dump_fixtures as dfx
    import dump_web_views as dwv

    for league in [x.strip() for x in args.leagues.split(",") if x.strip()]:
        dump_league(league, fixtures, out, tmp / "data", dfx, dwv)
    shutil.rmtree(tmp, ignore_errors=True)


def page_urls(league: str, ctx: Any, vals: dict, mine: Any, other: Any) -> dict[str, str]:
    """name -> URL; the Rust test derives the same list from the same data (see tests/app.rs)."""
    from urllib.parse import quote

    def best(team):  # noqa: ANN001
        ps = [p for p in team.players if p.cid in vals]
        return max(ps, key=lambda p: (vals[p.cid].fpg_season, p.cid), default=None)

    give, get = best(mine), best(other)
    first = next(p for p in mine.players)
    q = f"league={league}"
    return {
        "home": f"/?{q}",
        "plan": f"/plan?{q}",
        "overview": f"/overview?{q}",
        "overview_waiver": f"/overview?{q}&kind=waiver",
        "overview_fa": f"/overview?{q}&fa_pos=D&fa_sort=games&fa_q=an",
        "overview_flash": f"/overview?{q}&msg=mode-contend",
        "roster": f"/roster?{q}",
        "roster_stats": f"/roster?{q}&view=stats",
        "roster_other": f"/roster?{q}&team={quote(other.team_id)}",
        "recs": f"/recommendations?{q}",
        "recs_trade": f"/recommendations?{q}&kind=trade",
        "recs_flags": f"/recommendations?{q}&kind=flags",
        "recs_filtered": f"/recommendations?{q}&pos=D&min_gain=0.1",
        "news": f"/news?{q}",
        "player": f"/player/{quote(first.cid, safe=':')}?{q}",
        "player_give": f"/player/{quote(give.cid, safe=':')}?{q}",
        "player_missing": f"/player/nope?{q}",
        "schedule": f"/schedule?{q}",
        "schedule_week": f"/schedule?{q}&week=2026-11-18",
        "trade_eval": f"/trade-eval?{q}",
        "trade_eval_offer": f"/trade-eval?{q}&give={quote(give.cid)}&get={quote(get.cid)}",
        "trade_eval_switch": f"/trade-eval?{q}&team={quote(other.team_id)}&give={quote(give.cid)}&switch=1",
        "matchup": f"/matchup?{q}",
        "health": f"/health?{q}",
        "health_rollback_flash": f"/health?{q}&msg=rollback-unconfirmed",
        "api_recs": f"/api/recs.json?{q}",
        "api_recs_waiver": f"/api/recs.json?{q}&kind=waiver",
        "api_roster": f"/api/roster.json?{q}",
        "api_claims": f"/api/claims.json?{q}",
        "api_mode": "/api/mode.json",
        "healthz": "/healthz",
        "bad_league": "/overview?league=yahoo",
    }


def dump_league(league: str, fixtures: Path, out: Path, data_dir: Path, dfx: Any, dwv: Any) -> None:
    from fastapi.testclient import TestClient

    from fantasy_manager import cli
    from fantasy_manager.cache import HttpCache
    from fantasy_manager.config import get_settings
    from fantasy_manager.models import LeagueContext, Recommendation
    from fantasy_manager.providers.news import store_news
    from fantasy_manager.report.news_match import match_news_to_players
    from fantasy_manager.valuation.dynasty import DynastyValue
    from fantasy_manager.valuation.valuate import PlayerValue
    from fantasy_manager.valuation.rookie import RookieEstimate
    from fantasy_manager.web import app as webapp
    from fantasy_manager.web import views

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
    dwv.restore_registries(snap["extras"])
    from fantasy_manager.valuation import params as vparams
    vparams.reload()
    with vparams._lock:
        vparams._merged = snap["params"]["merged"]
        vparams._active = snap["params"]["active"]
        vparams._memo.clear()
    # POST /refresh etc. are not requested here: params stay as installed
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
    views.provider_for = lambda loader, lg: provider

    mine = ctx.my_team
    other = next(t for t in ctx.teams if not t.owner_is_me)
    first = next(p for p in mine.players)
    t0 = int(now.timestamp()) - 3 * 86400
    status_log = {first.cid: [SimpleNamespace(seen_at=t0, status="healthy", note=None),
                              SimpleNamespace(seen_at=t0 + 86400, status="dtd", note="upper body")]}
    news = match_news_to_players(store_news(data_dir, []), ctx.all_players())
    mode, source = webapp.current_mode()
    ctx = ctx.model_copy(update={"dynasty_mode": mode, "dynasty_mode_source": source})
    res = webapp.LoadResult(ctx=ctx, values=vals, dynasty=dyn, recs=recs, news_by_cid=news,
                            loaded_at=webapp.datetime.now(), warnings=list(ctx.warnings), mode=mode,
                            mode_source=source, compute_seconds=None, recalc_only=False, status_log=status_log)
    app = webapp.create_app(lambda lg: res, default_league=league, home="plan",
                            llm=lambda: SimpleNamespace(available=False))
    client = TestClient(app, raise_server_exceptions=False)
    pages: dict[str, Any] = {}
    for name, url in page_urls(league, ctx, vals, mine, other).items():
        r = client.get(url, follow_redirects=False)
        pages[name] = {"url": url, "status": r.status_code, "content_type": r.headers.get("content-type", ""),
                       "body": r.text}
        print(f"{league}: {name} {r.status_code} {len(r.text)}", flush=True)
    meta = {"now": snap["now"], "mode": mode, "mode_source": source, "first_cid": first.cid,
            "other_team": other.team_id}
    dfx.write_json(out / f"{league}.json", {"meta": meta, "pages": pages})


if __name__ == "__main__":
    main()
