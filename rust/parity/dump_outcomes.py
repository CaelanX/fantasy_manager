"""Grade a COPY of a harness data dir with Python's harness.outcomes and dump the outcome rows.

    python rust/parity/dump_outcomes.py <data dir copy> <out json> <as_of YYYY-MM-DD> [league ...]

Parity reference for fm_harness::outcomes (rows without ``graded_at``, ordered by outcome_id).
"""
from __future__ import annotations

import json
import sys
from datetime import date
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO))

from fantasy_manager.harness import Ledger  # noqa: E402
from fantasy_manager.harness.outcomes import grade_outcomes  # noqa: E402


def main() -> None:
    data_dir, out, as_of = Path(sys.argv[1]), Path(sys.argv[2]), date.fromisoformat(sys.argv[3])
    leagues = sys.argv[4:] or ["espn", "fantrax"]
    with Ledger(data_dir) as L:
        summaries = [grade_outcomes(L, lg, as_of).to_dict() for lg in leagues]
        rows = L.query("SELECT * FROM outcomes ORDER BY outcome_id")
    for r in rows:
        r.pop("graded_at", None)
    out.write_text(json.dumps({"summaries": summaries, "rows": rows}), encoding="utf-8")


if __name__ == "__main__":
    main()
