"""Parity reference for fm_harness::{metrics, health, track, refit} (W6-harness-metrics-refit).

    python rust/parity/dump_harness_metrics.py fixture   <data dir copy> <out json>
    python rust/parity/dump_harness_metrics.py synthetic <data dir> <rust data dir> <out json>

``fixture`` runs on a COPY of a harness data dir (the gitignored ``_datadir/harness.db``):
grade_week for every Monday 2026-09-28..2026-11-16 and league, status_report, the health pieces,
the track records, the refit replay table and a dry-run refit. ``synthetic`` builds the synthetic
ledger of tests/test_harness_refit.py in <data dir>, copies its harness.db to <rust data dir>
(the Rust side's starting state) and runs refit dry-runs, an applied refit and the graded weeks
with shadow scoring after it. Timestamps are left in (the Rust test masks them). The params
versions dir is ``<data dir>/harness/params`` (FM_PARAMS_DIR).
"""
from __future__ import annotations

import json
import math
import os
import shutil
import sys
from datetime import date, datetime, timedelta
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO))

from fantasy_manager.backtest.fit import InseasonObs  # noqa: E402
from fantasy_manager.harness import Ledger  # noqa: E402
from fantasy_manager.harness import health as H  # noqa: E402
from fantasy_manager.harness import metrics as M  # noqa: E402
from fantasy_manager.harness import refit as R  # noqa: E402
from fantasy_manager.harness import track as T  # noqa: E402
from fantasy_manager.harness.outcomes import store_scoring  # noqa: E402
from fantasy_manager.harness.params_store import ParamsStore  # noqa: E402
from fantasy_manager.models import ScoringConfig  # noqa: E402
from fantasy_manager.valuation import params as VP  # noqa: E402

TODAY = date(2026, 11, 4)
NOW = datetime(2026, 11, 4, 9, 30, 0)
MONDAYS = [date(2026, 9, 28) + timedelta(days=7 * i) for i in range(8)]


def clean(v):
    if isinstance(v, float) and (math.isnan(v) or math.isinf(v)):
        return None
    if isinstance(v, dict):
        return {str(k): clean(x) for k, x in v.items()}
    if isinstance(v, (list, tuple)):
        return [clean(x) for x in v]
    if isinstance(v, (date, datetime)):
        return v.isoformat()
    return v


def set_params_dir(data_dir: Path) -> None:
    os.environ[VP.OVERRIDE_ENV] = "1"
    os.environ[VP.PARAMS_DIR_ENV] = str(data_dir / "harness" / "params")
    VP.reload()


def replay_rows(obs):
    out = []
    for o in obs:
        out.append({"league": o.league, "snap": o.snap.isoformat(), "player": o.player, "pool": o.pool,
                    "status": o.status, "games": o.games, "exact": o.exact, "fpg_archived": o.fpg_archived,
                    "real_fpg28": o.real_fpg28, "gp28": o.gp28, "real_pts7": o.real_pts7, "gp7": o.gp7,
                    "fpg": R.inseason_projection(o), "proj_week": R.week_projection(o)})
    return out


def health_parts(L, leagues):
    out = {}
    for lg in leagues:
        _, latest = M.latest_rows(L, lg)
        out[lg] = {"series": H.series(L, lg), "scorecard": H.scorecard(latest), "capture": H.capture(L, lg, now=NOW),
                   "health_block": H.health_block(L, lg), "headline": M.headline(L, lg)}
    return out


def tables(L, data_dir: Path):
    snaps = L.query("SELECT * FROM metric_snapshots ORDER BY league, week, metric, pool")
    versions = L.query("SELECT * FROM param_versions ORDER BY version")
    runs = L.query("SELECT command, league, as_of, status, summary_json FROM runs WHERE command='auto-rollback'")
    pdir = data_dir / "harness" / "params"
    files = {p.name: json.loads(p.read_text(encoding="utf-8")) for p in sorted(pdir.glob("*.json"))} if pdir.exists() else {}
    return {"metric_snapshots": snaps, "param_versions": versions, "rollback_runs": runs, "params_files": files}


def fixture(data_dir: Path, out: Path) -> None:
    set_params_dir(data_dir)
    res = {"grades": []}
    with Ledger(data_dir) as L:
        leagues = M.known_leagues(L)
        res["leagues"] = leagues
        for wk in MONDAYS:
            for lg in leagues:
                res["grades"].append(M.grade_week(L, lg, wk))
        res["status"] = M.status_report(L)
        res["health"] = health_parts(L, leagues)
        res["changelog"] = H.changelog(ParamsStore(ledger=L))
        res["calendar"] = H.calendar(TODAY, data_dir)
        res["track"] = {lg: T.as_json(T.TrackBook.load(data_dir, lg, TODAY)) for lg in leagues}
        res["track_strip"] = {lg: T.strip(T.TrackBook.load(data_dir, lg, TODAY)) for lg in leagues}
        obs, info = R.replay_table(L, date(2026, 11, 30))
        res["replay_info"] = info
        res["replay"] = replay_rows(obs)
        res["refit"] = R.run_refit(L, date(2026, 11, 16), mode="dry-run").to_dict()
        res["refit_locked"] = R.run_refit(L, date(2026, 10, 20), mode="dry-run").to_dict()
        res.update(tables(L, data_dir))
    out.write_text(json.dumps(clean(res)), encoding="utf-8")


# --------------------------------------------------------------------------- synthetic (test_harness_refit.py)

SC = ScoringConfig(kind="points", weights={"G": 1.0, "A": 0.5, "W": 2.0, "SV": 0.1, "GA": -1.0})
FIRST_SNAP = date(2026, 10, 5)
N_WEEKS = 6
N_PLAYERS = 80
AS_OF = date(2026, 12, 9)


def _rate(i):
    return 0.2 + 0.02 * i


def build_ledger(L, n_players=N_PLAYERS, n_weeks=N_WEEKS, proj_bias=0.6):
    store_scoring(L, "espn", SC)
    rows = []
    means = {"F": {"G": 1.0, "A": 0.0}}
    for w in range(n_weeks):
        snap = FIRST_SNAP + timedelta(days=7 * w)
        L.execute("INSERT INTO meta(key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                  (f"means:espn:{snap.isoformat()}", json.dumps(means)))
        gp = 5 + 3 * w
        for i in range(n_players):
            r = _rate(i)
            proj = r + proj_bias * (1.0 - r)
            pos = "G" if i % 10 == 9 else ("D" if i % 5 == 3 else "C")
            inputs = {"season": {"gp": gp, "stats": {"G": r * gp, "A": 0.0, "GP": gp}, "zeros": True},
                      "last15": {"gp": min(gp, 7), "stats": {"G": (r + 0.1) * min(gp, 7)}},
                      "history": {"rates": {"G": 0.8, "A": 0.1}, "gp": 120} if i % 3 == 0 else None,
                      "projection": {"gp": 82, "stats": {"G": proj * 82, "GP": 82}},
                      "status": ["healthy", "dtd", "out"][i % 3] if i % 7 == 0 else "healthy",
                      "games_next7": 4 if i % 11 else 3, "offnight_next7": i % 3, "start_share": None,
                      "positions": [pos]}
            rows.append({"league": "espn", "as_of": snap.isoformat(), "cid": f"p{i}", "name": f"p{i}",
                         "nhl_id": 1000 + i, "positions": pos, "status": "healthy", "fpg": r * (1 + 0.04 * ((i + w) % 5 - 2)), "proj_week": 4 * r,
                         "fpg_week": r, "inputs_json": json.dumps(inputs), "archive_version": 2})
    L.upsert("projections", rows, ("league", "as_of", "cid"))
    last = FIRST_SNAP + timedelta(days=7 * (n_weeks - 1) + 40)
    days = [FIRST_SNAP + timedelta(days=k) for k in range(1, (last - FIRST_SNAP).days + 1)]
    L.upsert("realized_pulls", [{"game_date": d.isoformat(), "n_players": n_players} for d in days], ("game_date",))
    L.upsert("realized_daily", [{"nhl_id": 1000 + i, "game_date": d.isoformat(), "gp": 1,
                                 "stats_json": json.dumps({"G": _rate(i) * (1.0 + 0.3 * ((k + i) % 3 - 1))})}
                                for i in range(n_players) for k, d in enumerate(days) if k % 2 == 0],
             ("nhl_id", "game_date"))
    return L


def hist_flat(n=200):
    return [InseasonObs(season=2024, day="11-01", player_id=i, group="F", gp_td=0, fpg_td=0.0, fpg_base=1.0,
                        has_base=True, splits={}, gp_rest=40, fpg_rest=1.0 + (0.1 if i % 2 else -0.1))
            for i in range(n)]


def hist_prefers_high_k(n=200):
    return [InseasonObs(season=2024, day="11-01", player_id=i, group="F", gp_td=10,
                        fpg_td=1.0 + (0.5 if i % 2 else -0.5), fpg_base=1.0, has_base=True, splits={}, gp_rest=40,
                        fpg_rest=1.0) for i in range(n)]


def synthetic(data_dir: Path, rust_dir: Path, out: Path) -> None:
    data_dir.mkdir(parents=True, exist_ok=True)
    with Ledger(data_dir) as L:
        build_ledger(L)
    rust_dir.mkdir(parents=True, exist_ok=True)
    shutil.copy(data_dir / "harness.db", rust_dir / "harness.db")
    set_params_dir(data_dir)
    res = {"refit": {}}
    with Ledger(data_dir) as L:
        store = ParamsStore(ledger=L)
        obs, info = R.replay_table(L, AS_OF)
        res["replay_info"] = info
        res["replay"] = replay_rows(obs)
        res["refit"]["dry"] = R.run_refit(L, AS_OF, mode="dry-run", store=store, hist={"espn": hist_flat()}).to_dict()
        res["refit"]["tier_b"] = R.run_refit(L, AS_OF, mode="propose", store=store, hist={"espn": hist_flat()},
                                             only=["availability", "start_share", "offnight_bonus"]).to_dict()
        old = R.HIST_POOL_K
        R.HIST_POOL_K = 10
        try:
            res["refit"]["hist_worse"] = R.run_refit(L, AS_OF, mode="apply", store=store,
                                                     hist={"espn": hist_prefers_high_k()}, only=["k_inseason"]).to_dict()
        finally:
            R.HIST_POOL_K = old
        res["refit"]["auto_tier_b"] = R.run_refit(L, AS_OF, mode="auto", store=store, hist={"espn": hist_flat()},
                                                  only=["availability", "offnight_bonus"]).to_dict()
        res["refit"]["apply"] = R.run_refit(L, AS_OF, mode="apply", store=store, hist={"espn": hist_flat()}).to_dict()
        res["hash_after_apply"] = VP.params_hash()
        # a worse version promoted since 2026-10-01: shadow scoring rolls it back after two weeks
        knobs = {**R.current_knobs(), "k_inseason.skater": 60.0}
        rec = store.propose(R.knobs_override(knobs, ["k_inseason.skater"]), ["k_inseason.skater"])
        store.apply(rec["version"], as_of=date(2026, 10, 1))
        res["hash_bad"] = VP.params_hash()
        res["grades"] = [M.grade_week(L, "espn", wk) for wk in (date(2026, 11, 9), date(2026, 11, 9),
                                                                date(2026, 11, 16), date(2026, 11, 23),
                                                                date(2026, 12, 7))]
        res["status"] = M.status_report(L)
        res["health"] = health_parts(L, ["espn"])
        res["changelog"] = H.changelog(ParamsStore(ledger=L))
        res["hash_final"] = VP.params_hash()
        res.update(tables(L, data_dir))
    out.write_text(json.dumps(clean(res)), encoding="utf-8")


def main() -> None:
    mode = sys.argv[1]
    if mode == "fixture":
        fixture(Path(sys.argv[2]), Path(sys.argv[3]))
    elif mode == "synthetic":
        synthetic(Path(sys.argv[2]), Path(sys.argv[3]), Path(sys.argv[4]))
    else:
        raise SystemExit(f"unknown mode {mode}")


if __name__ == "__main__":
    main()
