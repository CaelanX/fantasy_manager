"""Golden generator for the Rust matchup-calibration parity test (W8-outlook-playoffs-calib).

harness.db has no matchup rows, so this seeds synthetic ones (deterministic) into a temp copy of
fixtures/_datadir/harness.db with the Python recorders, then runs harness.matchup_calib on it.
Writes <out>/matchup_calib.json:
  {"seed": {"snapshots": [row...], "results": [row...]},     # raw rows the Rust test inserts
   "leagues": {league: {"report", "summary", "warning", "pending"(today)}},
   "snapshot_row": {"input": {...}, "row": {...}}}            # snapshot_row parity case
Usage: python dump_matchup_calib.py <worktree> <fixtures_dir> <out_dir> [today=2026-11-04]
"""
import json
import random
import shutil
import sys
import tempfile
from datetime import date, timedelta
from pathlib import Path
from types import SimpleNamespace

wt, fx, out = Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3])
today = date.fromisoformat(sys.argv[4]) if len(sys.argv) > 4 else date(2026, 11, 4)
sys.path.insert(0, str(wt))

from fantasy_manager.harness.ledger import Ledger  # noqa: E402
from fantasy_manager.harness import matchup_calib as mc  # noqa: E402
from fantasy_manager.harness.health import _matchup_warning  # noqa: E402


def mu(period, start, end, p, mine, theirs, sds, days_left, opp="Opp"):
    return SimpleNamespace(period=period, start=start, end=end, opponent_team=opp, opponent_id="t2",
                           days_left=days_left, playoffs=False, my_points_so_far=mine[0],
                           my_projected_remaining=mine[1], their_points_so_far=theirs[0],
                           their_projected_remaining=theirs[1], my_sd=sds[0], their_sd=sds[1], win_probability=p,
                           source="test")


tmp = Path(tempfile.mkdtemp())
shutil.copy(fx / "_datadir" / "harness.db", tmp / "harness.db")
rng = random.Random(11)
res = {}
with Ledger(tmp) as led:
    led.execute("DELETE FROM matchup_snapshots")
    led.execute("DELETE FROM matchup_results")
    led.commit()
    for lg, n in (("espn", 24), ("fantrax", 5)):
        for i in range(n):
            start = date(2026, 9, 28) + timedelta(days=7 * i)
            end = start + timedelta(days=6)
            true_margin = rng.gauss(0, 14)
            for k in range(1, 6):
                day = start + timedelta(days=k)
                dl = (end - day).days + 1
                so_far = (k * 11.0, k * 10.0)
                rem = (rng.uniform(5, 60) * dl / 5, rng.uniform(5, 60) * dl / 5)
                sd = (rng.uniform(2, 9), rng.uniform(2, 9))
                m = mu(i + 1, start, end, min(1.0, max(0.0, 0.5 + rng.gauss(0, 0.3))), (so_far[0], rem[0]),
                       (so_far[1], rem[1]), sd, dl)
                mc.record_snapshot(led, lg, day, m)
            # an end-of-period snapshot with nothing left to play (not graded)
            mc.record_snapshot(led, lg, end, mu(i + 1, start, end, 1.0, (80.0, 0.0), (70.0, 0.0), (0.0, 0.0), 0))
            if start + timedelta(days=6) < today - timedelta(days=0) or lg == "espn":
                mine = 90.0 + rng.uniform(-20, 20)
                theirs = mine - true_margin
                if i == 3:
                    theirs = mine  # a tie
                led.upsert("matchup_results", [{
                    "league": lg, "period": i + 1, "period_end": end.isoformat(), "period_start": start.isoformat(),
                    "opponent_id": "t2", "my_final": mine, "their_final": theirs, "result": mc.result_of(mine, theirs),
                    "source": "test", "recorded_at": "2026-11-04T00:00:00"}], ["league", "period", "period_end"])
    # pending: a snapshot for a finished period without a result
    mc.record_snapshot(led, "pending", date(2026, 11, 1), mu(1, date(2026, 10, 27), date(2026, 11, 2), 0.5,
                                                             (1.0, 2.0), (1.0, 2.0), (3.0, 3.0), 2))
    for lg in ("espn", "fantrax", "pending", "nonexistent"):
        rep = mc.calibration_report(led, lg)
        res[lg] = {"report": rep, "summary": mc.summary_line(rep), "warning": _matchup_warning(led, lg),
                   "pending": mc.pending_periods(led, lg, today)}
    seed = {"snapshots": led.query("SELECT * FROM matchup_snapshots ORDER BY league, day"),
            "results": led.query("SELECT * FROM matchup_results ORDER BY league, period")}

m = mu(7, date(2026, 11, 2), date(2026, 11, 8), 0.61234, (50.5, 31.25), (45.0, 29.5), (3.5, 4.25), 4)
row = mc.snapshot_row("espn", date(2026, 11, 4), m)
row.pop("recorded_at")
case = {"input": {"period": 7, "start": "2026-11-02", "end": "2026-11-08", "p": 0.61234, "mine": [50.5, 31.25],
                  "theirs": [45.0, 29.5], "sds": [3.5, 4.25], "days_left": 4}, "row": row}
out.mkdir(parents=True, exist_ok=True)
(out / "matchup_calib.json").write_text(
    json.dumps({"seed": seed, "leagues": res, "snapshot_row": case, "today": today.isoformat()}, indent=1,
               default=str), encoding="utf-8")
print({k: (v["report"]["snapshots"], v["report"]["graded_snapshots"], v["report"]["matchups"], v["report"]["status"],
           v["warning"]) for k, v in res.items()})
