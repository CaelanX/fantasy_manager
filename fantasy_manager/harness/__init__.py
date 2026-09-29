"""Self-correcting evaluation harness (see docs/harness.md and docs/harness-plan.md).

M1 (capture): the sqlite ledger, archive ingest, nightly realized / lineup / transaction pulls
and matching of recommendation episodes to what I actually did.
M2 (grading): ``outcomes.grade_outcomes`` (realized gain per episode / move and window) and
``metrics.grade_week`` / ``status_report`` / ``headline`` (the bar, with trust labels).
M3 (auto-correction): ``params_store`` (versioned params overrides, apply / rollback) and
``refit`` (replay table, bounded search, the gate, champion / challenger shadow scoring).
"""
from .ingest import EPISODE_GAP_DAYS, ingest_archive, record_run, roll_episodes
from .ledger import DB_NAME, TABLES, Ledger, rec_key
from .match import (EpisodeRow, LineupRow, MatchResult, MatchSummary, TxRow, first_lockable_day, match_episodes,
                    match_rows, window_end)
from .metrics import grade_week, headline, status_report
from .outcomes import grade_outcomes, scoring_config, store_scoring
from .realized import pull_lineups, pull_realized, pull_transactions

__all__ = [
    "DB_NAME", "EPISODE_GAP_DAYS", "TABLES", "EpisodeRow", "Ledger", "LineupRow", "MatchResult", "MatchSummary",
    "TxRow", "first_lockable_day", "grade_outcomes", "grade_week", "headline", "ingest_archive", "match_episodes",
    "match_rows", "pull_lineups", "pull_realized", "pull_transactions", "rec_key", "record_run", "roll_episodes",
    "scoring_config", "status_report", "store_scoring", "window_end",
]
