"""Self-correcting evaluation harness (see docs/harness.md and docs/harness-plan.md).

M1 (capture): the sqlite ledger, archive ingest, nightly realized / lineup / transaction pulls
and matching of recommendation episodes to what I actually did.
"""
from .ingest import EPISODE_GAP_DAYS, ingest_archive, record_run, roll_episodes
from .ledger import DB_NAME, TABLES, Ledger, rec_key
from .match import (EpisodeRow, LineupRow, MatchResult, MatchSummary, TxRow, first_lockable_day, match_episodes,
                    match_rows, window_end)
from .realized import pull_lineups, pull_realized, pull_transactions

__all__ = [
    "DB_NAME", "EPISODE_GAP_DAYS", "TABLES", "EpisodeRow", "Ledger", "LineupRow", "MatchResult", "MatchSummary",
    "TxRow", "first_lockable_day", "ingest_archive", "match_episodes", "match_rows", "pull_lineups",
    "pull_realized", "pull_transactions", "rec_key", "record_run", "roll_episodes", "window_end",
]
