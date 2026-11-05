"""Python goldens for W6: goalie_week (and a re-run of matchup waivers as a reconstruction check)
from the parity snapshot, plus Python timings. Usage: python golden_goalie_week.py <worktree> <fixtures> <out_dir>"""
import datetime as dt
import json
import os
import shutil
import sys
import tempfile
import time
from pathlib import Path

wt, fixtures, out = Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3])
sys.path.insert(0, str(wt))
sys.path.insert(0, str(wt / "rust" / "parity"))
tmp = Path(tempfile.mkdtemp())
shutil.copytree(fixtures / "_datadir", tmp / "data", ignore=shutil.ignore_patterns("fantrax_cache", "harness.db"))
os.environ["FM_DATA_DIR"] = str(tmp / "data")
os.environ.setdefault("FM_NEWS_ROLES_LLM", "0")

import dump_fixtures as dfx  # noqa: E402

from fantasy_manager.models import LeagueContext  # noqa: E402
from fantasy_manager.valuation.valuate import PlayerValue  # noqa: E402
from fantasy_manager.valuation.rookie import RookieEstimate  # noqa: E402
from fantasy_manager.valuation.dynasty import DynastyValue  # noqa: E402

out.mkdir(parents=True, exist_ok=True)
for league in ("espn", "fantrax"):
    snap = json.loads((fixtures / league / "snapshot.json").read_text(encoding="utf-8"))
    now = dt.datetime.fromisoformat(snap["now"])
    dfx.freeze_clock(now)
    for k, v in (snap.get("env") or {}).items():
        if v is None:
            os.environ.pop(k, None)
        else:
            os.environ[k] = v
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
    from fantasy_manager.analysis.goalie_plan import goalie_week
    from fantasy_manager.recommend.waivers import recommend_matchup_waivers, recommend_waivers
    from fantasy_manager.recommend.lineup import recommend_lineup
    t = time.perf_counter()
    gw = goalie_week(ctx, vals, data_dir=tmp / "data", dynasty_values=dyn)
    t_gw = time.perf_counter() - t
    dfx.write_json(out / f"{league}_goalie_week.json", dfx.jsonable(gw))
    t = time.perf_counter()
    mw = recommend_matchup_waivers(ctx, vals, dynasty_values=dyn)
    t_mw = time.perf_counter() - t
    t = time.perf_counter()
    w = recommend_waivers(ctx, vals, dynasty_values=dyn)
    t_w = time.perf_counter() - t
    t = time.perf_counter()
    lu = recommend_lineup(ctx, vals)
    t_lu = time.perf_counter() - t
    exp = json.loads((fixtures / league / "engines" / "matchup_waivers.json").read_text(encoding="utf-8"))
    same = json.dumps(dfx.jsonable(mw), sort_keys=True) == json.dumps(exp, sort_keys=True)
    print(f"{league}: goalie_week {t_gw:.2f}s, matchup_waivers {t_mw:.2f}s (matches fixture: {same}), "
          f"waivers {t_w:.2f}s, lineup {t_lu:.2f}s", flush=True)
