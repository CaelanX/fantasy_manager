"""Write the Python archive files for a fixture league (parity reference for fm_engine::archive).

    python rust/parity/dump_archive.py <fixtures/league dir> <out data dir>

Loads ``snapshot.json`` (ctx) and ``values.json`` of the league, builds the PlayerValue stand-ins
from values.json and writes ``<out>/archive/projections-<provider>-<as_of>.json`` and
``recs-<provider>-<as_of>.json`` (two synthetic recommendations, mirrored in
``crates/fm-harness/tests/outcomes.rs``) with ``backtest.archive``. Run with the venv python.
"""
from __future__ import annotations

import json
import sys
from datetime import date, datetime
from pathlib import Path
from types import SimpleNamespace

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO))

from fantasy_manager.backtest import archive as A  # noqa: E402
from fantasy_manager.models import LeagueContext, Reason, Recommendation  # noqa: E402


def synthetic_recs(ctx: LeagueContext) -> list[Recommendation]:
    mine = ctx.my_team.players
    fa = ctx.free_agents
    return [
        Recommendation(kind="waiver", score=0.73456, title="Add A, drop B", add=[fa[0]], drop=[mine[0]],
                       predicted_gain=0.951234, gain_units="season_fpg", strength=6.2468,
                       reasons=[Reason(code="VORP_DELTA", text="x é", value=0.95, baseline=0.1),
                                Reason(code="PLAIN", text="y")]),
        Recommendation(kind="trade", score=1.5, title="Trade", add=[fa[1]], drop=[mine[1]], counterparty="2",
                       subjects=[mine[2]], predicted_gain=2.0, gain_units="week_pts", horizon_days=7,
                       strength=3.0, reasons=[]),
    ]


def main() -> None:
    league_dir, out = Path(sys.argv[1]), Path(sys.argv[2])
    snap = json.loads((league_dir / "snapshot.json").read_text(encoding="utf-8"))
    ctx = LeagueContext.model_validate(snap["ctx"])
    raw = json.loads((league_dir / "values.json").read_text(encoding="utf-8"))
    values = {cid: SimpleNamespace(**v) for cid, v in raw.items()}
    out.mkdir(parents=True, exist_ok=True)
    p1, s1 = A.archive_projections(ctx, out, values)
    p2, s2 = A.archive_recommendations(synthetic_recs(ctx), ctx.provider, out, as_of=ctx.as_of,
                                       league_id=ctx.league_id)
    print(json.dumps({"projections": str(p1), "recs": str(p2), "status": [s1, s2]}))


if __name__ == "__main__":
    main()
