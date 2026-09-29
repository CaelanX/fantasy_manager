# Evaluation harness

The harness keeps a running record of what the model recommended, what you actually did and what
then happened in the NHL, so recommendations and projections can be graded and a small set of
valuation parameters corrected over time. The plan of record is `docs/harness-plan.md`; this page
describes what exists now (milestone 1, "capture").

## Daily use

```
fm harness daily            # both leagues; --league espn|fantrax for one
fm harness status           # what the ledger holds
fm harness ledger           # recent recommendation episodes and their match status
fm harness rebuild          # rebuild the archive-derived tables from data/archive
```

`fm harness daily` runs, per league:

1. **Archive** today's projections and recommendations (`fm backtest archive`) unless today's
   files already exist (`--force-archive` re-archives, `--no-archive` skips).
2. **Ingest** new or changed archive files into the ledger and roll recommendations into episodes.
3. **Pull transactions** since 7 days ago (`--since`): adds, drops, trades and (Fantrax) pending
   trade proposals, for every team.
4. **Pull lineups**: ESPN box scores for yesterday (every team's slots and ESPN points); Fantrax
   a snapshot of today's rosters (Fantrax lineups are weekly, locked on Monday).
5. **Match** episodes to your moves (below).

Then it pulls yesterday's NHL results once for the whole league (`--realized-day` to choose the
date). The whole run is idempotent: a second run the same day changes nothing. A day without NHL
regular-season games (preseason, All-Star break) is a clean no-op that reports 0 players.
Provider problems become warnings in the output, never a failed run for the other league.

It is scheduled right after `fm backtest archive` (see `docs/scheduling.md`). Run it every day:
the ESPN activity feed pages back 25 moves at a time (the harness reads up to 10 pages) and
Fantrax only returns the last 50 transactions, so moves fall off the end if you skip days.

## What is captured

Everything lives in `<FM_DATA_DIR>/harness.db` (sqlite). The JSON archive under
`<FM_DATA_DIR>/archive/` stays the source of truth for projections and recommendations; the db
can be rebuilt from it. Pulled data (transactions, lineups, NHL results) exists only in the db,
so `fm harness rebuild` keeps it unless you pass `--all`.

**Archive v2** (`backtest/archive.py`, `ARCHIVE_VERSION = 2`). Each file header carries
`params_hash` (sha1 of the loaded valuation params) and `code_hash` (sha1 over
`valuation/*.py` and `recommend/*.py`). Each projection record adds an `inputs` block with what
the valuation read that day: season-to-date and last-30/15/7 lines (zero stats omitted), the
multi-season history baseline rates and its GP, the provider projection, status and note,
games and off-night games in the next 7 days, goalie start share, birth date and age, pct
owned and positions. The `fm` block adds `fpg_season`, `fpg_week`, `proj_week` and `vorp`.
Projection files are written without indentation to keep them small. Version 1 files (before
2026-09-28 evening) are still read everywhere.

**Recommendations** now carry an explicit predicted gain:

| kind | `predicted_gain` | `gain_units` | `horizon_days` |
|---|---|---|---|
| waiver | add FPG minus the drop's (or the benched player's) | `season_fpg` | none (rest of season) |
| waiver, week horizon | projected week points difference | `week_pts` | 7 |
| lineup | lineup gain from the solver | `week_pts` (or `season_fpg` without a schedule) | 7 |
| trade | ΔMe, my optimal lineup's FPG change (dynasty Δ stays a reason) | `lineup_fpg` | none |
| injury (IR move + add) | the waiver add's gain | as the waiver | as the waiver |
| injury (activation) | his season FPG minus the drop's | `season_fpg` | none |
| sell_high / buy_low | expected FPG change from L15 form back to the shrunk baseline (negative for sell-high) | `season_fpg` | 28 |

`subjects` lists players a rec acts on without adding or dropping them (the IR target of an IR
move, the player of a status alert). `strength` is an absolute 0-10 score from the predicted
gain on fixed per-kind scales (`recommend/strength.py`), and `rank_in_kind` / `kind_total`
give "1 of 4 waivers"; `score` keeps its engine meaning (advise rescales it by rank).

**Ledger tables.** `recs` / `rec_players` (every archived rec per day, keyed by
`rec_key = sha1(league|kind|sorted adds|sorted drops|counterparty)`, plus the sorted subjects
when there are any), `rec_episodes`, `transactions`, `lineup_days`, `projections` (fm numbers +
inputs), `realized_daily` (raw NHL stats per player per game date; scored later with each
league's own scoring), `decisions`, and `runs`. `outcomes`, `metric_snapshots` and
`param_versions` exist but stay empty until grading (M2) and refitting (M3). Bookkeeping:
`archive_files` (sha1 of each ingested file, so unchanged files are skipped) and
`realized_pulls` (which game dates were pulled, including empty ones).

## Episodes and matching

The same rec seen on consecutive days (a gap of up to 2 days is tolerated, for a missed run) is
one **episode** with `first_seen`, `last_seen` and `n_days`. Episodes are graded from
`first_seen`; the acted-on date is kept for a secondary grade.

Only your own moves count. Windows run from `first_seen`:

| kind | window | followed | partial |
|---|---|---|---|
| waiver | to `last_seen` + 3 days | you added the player and the drops in that transaction equal the rec's | you added him with a different (or no) drop |
| trade | to `last_seen` + 7 days | a trade with exactly the rec's give and get | a trade sharing at least one give and one get (e.g. a 2-for-1 you made 1-for-1) |
| lineup | first lockable day | every `add` starts and every `drop` sits | only one side changed |
| injury | to `last_seen` + 2 days | the player is in IR (IR move or IR slot) and the add was made | one of the two |
| sell_high / buy_low | to `last_seen` + 14 days | you traded the player away / for him | |

The first lockable day is the rec's own day on ESPN (daily locks; graded on that day's box
score, pulled the next morning, with the next day as a fallback) and the next Monday on Fantrax
(the same day if issued on a Monday). Fantrax is graded on the first roster snapshot taken after
that Monday (Tuesday to Sunday), because the Monday-morning run predates the lock. A trade you only proposed (Fantrax pending
trades) marks the episode `proposed`, which can still become `followed`.

Statuses: `open` (window still running), `followed`, `partial`, `proposed`, `expired` (the window
passed without a match, or the lineup at the lock did not change; graded as "ignored" later).
Every matched episode becomes a `decisions` row with that origin; your transactions that no
episode explains become `decisions(origin=user_only)` (the "your moves vs the model's"
comparison in M2). Other teams' moves stay in `transactions` as league-wide samples.

Matching is recomputed from scratch on every run, so it never depends on the order data
arrived in.

## What `status` shows now and later

Now (M1): counts only. Episodes by league, kind and status; projection rows and days (and how
many carry v2 inputs); transaction rows (yours vs other teams); decisions by origin; lineup rows
and days; NHL result days pulled and how many had games; the last run.

Later: M2 adds grading (`fm harness grade`), projection accuracy against realized FPG with
skill scores and confidence intervals, recommendation hit rates for followed / ignored /
user-only moves, calibration and trust labels (nothing is shown as reliable before enough
weeks have matured: skater projections roughly week 4-8, goalies and trades not this season).
M3 adds the bounded auto-correction of parameters (`refit`, `rollback`, `params`), and M4 the
`/health` dashboard page.

## Known limits

- ESPN's activity feed has no IR or lineup messages; IR moves are detected from lineup slots.
- Fantrax trade rows are read as "the player moved to this row's team"; the giving team is
  inferred when exactly two teams are in the trade.
- Fantrax lineups can only be captured as the current roster at run time, so a week's lineup
  is known once `fm harness daily` has run on a day after the Monday lock.
- v1 recommendations (2026-09-28) have no predicted gain or subjects; IR targets are recovered
  from the title and that day's projections.
