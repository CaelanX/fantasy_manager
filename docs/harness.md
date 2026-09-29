# Evaluation harness

The harness keeps a running record of what the model recommended, what you actually did and what
then happened in the NHL, so recommendations and projections can be graded and a small set of
valuation parameters corrected over time. The plan of record is `docs/harness-plan.md`; this page
describes what exists now (milestone 1, "capture", milestone 2, "grading and the bar", and
milestone 3, "auto-correction").

## Daily use

```
fm harness daily            # both leagues; --league espn|fantrax for one
fm harness status           # the bar (accuracy, hit rates, trust labels) and what the ledger holds
fm harness grade            # grade matured outcomes + this week's bar (--week YYYY-MM-DD, --league)
fm harness ledger           # recent recommendation episodes and their match status
fm harness rebuild          # rebuild the archive-derived tables from data/archive
fm harness params           # active valuation params and versions (refit / rollback: M3)
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

It also stores each league's ScoringConfig in the ledger (`meta`, used to score realized
stats at grade time). Then it pulls yesterday's NHL results once for the whole league (`--realized-day` to choose the
date). The whole run is idempotent: a second run the same day changes nothing. A day without NHL
regular-season games (preseason, All-Star break) is a clean no-op that reports 0 players.
On Mondays (or with `--grade`) it finishes with `fm harness grade`. On refit days (every 14 days from 2026-11-02) it
also runs the params refit once (see "Auto-correction (M3)").
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
the valuation read that day: season-to-date and last-30/15/7 lines (zero stats kept from
2026-09-29, `"zeros": true`; earlier files omitted them), the
multi-season history baseline rates and its GP, the provider projection, status and note,
games and off-night games in the next 7 days, goalie start share, birth date and age, pct
owned and positions, and for goalies the start-share parts (`share`). The header also carries
`position_means` (the means a league projection is shrunk toward). The `fm` block adds `fpg_season`, `fpg_week`, `proj_week` and `vorp`.
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
league's own scoring), `decisions`, and `runs`. Grading fills `outcomes` and
`metric_snapshots`; `param_versions` mirrors the params versions (M3). Bookkeeping:
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

## Grading (M2)

`fm harness grade` (and `daily` on Mondays) first grades **outcomes**, then computes the week's
**bar** (`metric_snapshots`). Every command takes `--json`; `/api/health.json` and the
dashboard's Health tab show the same report.

**Outcomes** (`harness/outcomes.py`, one row per episode or move, basis and window). Realized
stats from `realized_daily` are scored with the league's own ScoringConfig (`meta`, else the
newest archive header, else the preset). Windows are the 7 and 28 days after the basis day and
rest of season so far; the basis is the rec's `first_seen` (plus a secondary grade from the day
you acted) or the day of your own move. A window is `complete` only when it is fully in the
past and every day of it was pulled; otherwise the row is stored with `complete=0` and regraded
next time.

| kind | realized gain |
|---|---|
| waiver, injury, your own adds | points of the added player(s) minus the dropped |
| trade | get minus give, minus (n get - n give) x the realized points of replacement-level players (vorp ~ 0 that day) |
| lineup | 7 days: ESPN from the rec's day, only days both sides played; Fantrax the locked week (Monday to Sunday) |
| sell_high / buy_low | 28 days: realized FPG minus the L15 FPG at flag time; sell-high hits below 0, buy-low above |

Predicted gains are converted to points over the window (`predicted_pts`: `week_pts` x days/7,
per-game units x expected games). Your user-only moves also record the model's view of the
move that day (fpg in - fpg out) and the model's own pick that day, realized over the same
window (the counterfactual).

**The bar** (`harness/metrics.py`, as of a Monday; only windows that ended before it count):

- Projection accuracy per pool (F / D / G), from one projection snapshot per ISO week: MAE,
  Spearman and bias of `fpg` vs realized FPG over the next 28 days (players with >= 8 GP), the
  baselines (season-to-date, frozen preseason, provider projection, last season) and the skill
  score 1 - MAE_fm / MAE_to_date with a 95% player-clustered bootstrap CI; `proj_week` vs the
  next 7 days' points, split into rate error and availability error.
- Hit rates per kind and origin (followed / partial / ignored / user_only) with Wilson 95% CIs
  and the mean realized gain; calibration of predicted vs realized points by decile (terciles
  under 100); your moves vs the model's; trades as a case list, never aggregated.
- Trust labels: projections hidden under 150 player-windows per pool, provisional from 150,
  reliable from 400 over at least 4 weekly snapshots (goalies at most provisional before
  January); hit rates hidden under 20, provisional 20-49, reliable from 50; calibration needs
  100. `status` lists what is not judgeable yet and how many more observations each needs.
- The digest (`fm report`) gets one "Model: ..." line from the trustworthy metrics; it stays
  hidden while everything is hidden.

M4 adds the full `/health` page (sparklines, scorecard, params changelog).

## Auto-correction (M3)

```
fm harness params [--history]          # active params (packaged fit + harness version), refittable values
fm harness refit --dry-run             # fit + gate, write nothing
fm harness refit [--only g1,g2]        # store a passing candidate as a proposed version
fm harness refit --apply               # ... and activate it
fm harness refit --force               # bypass the calendar lock only (never the gate)
fm harness rollback [--to vNNNN|packaged]
```

All take `--json`; `refit` also takes `--league` (restrict the replay table) and `--as-of`.

**Params layering** (`valuation/params.py`). `load_params()` is the packaged
`valuation/fitted_params.json` deep-merged with the active harness version:
`<FM_DATA_DIR>/harness/params/active.json` (`{"version": "v0003"}`) points at `v0003.json`,
whose `params` holds only the overridden keys in the same nested shape. `FM_PARAMS_OVERRIDE=0`
disables the layer (the test suite does this in `tests/conftest.py`); a missing, unreadable or
malformed pointer or version file is ignored, and an invalid value falls back per accessor.
The merged result is cached until `params.reload()`, which the dashboard's **Refresh data**
calls (every CLI run is a fresh process). `params_hash()` hashes the merged result, so archive
headers identify the version a snapshot was made with. `source()` reports the provenance
("fit 2026-09-28 (espn) + harness v0003 (2026-11-17)"); `fm settings` and the dashboard
footer show it. Live code reads every tunable through accessors at call time:
`k_inseason(group)`, `recency_weights()`, `projection_weight()`, `k_projection(group)`,
`availability(status, horizon)`, `start_share_prior()`, `start_share_k()`,
`offnight_bonus()` (each with a hard-coded fallback). The old module constants
(`blend.K_INSEASON`, `RECENCY_WEIGHTS`, `PROJECTION_WEIGHT`, `K_PROJECTION`,
`adjust.AVAILABILITY`, `schedule.OFFNIGHT_BONUS` / `START_SHARE_*`) remain as import-time
snapshots for the backtest and report only.

**Versions** (`harness/params_store.py`). Each `vNNNN.json` holds `version`, `parent`
(`packaged` for the first), `created`, `params` (cumulative: the parent's overrides plus this
cycle's changes), `hash`, `changed_keys`, `metrics` (`holdout_before/after`,
`hist_before/after`, `n_live`, per-objective detail), `status`, `applied_by`, `applied_at`,
`changelog` and `shadow`. Statuses: `proposed`, `active`, `shadow` (the version an active one
replaced), `rolled_back`, and `retired` (an older shadow after a newer promotion). Every
write is mirrored into the ledger's `param_versions` table (`rebuild` and `params` re-sync it
from the files, which are the source of truth). `rollback` goes to the parent by default and
restores the exact previous params hash.

**What can change.** Tier A (auto-apply allowed): `k_inseason` {skater, goalie},
`recency_weights`, `projection_weight`, `k_projection` {skater, goalie}. Tier B (proposals only
until 2026-12-01, never auto-applied): availability week multipliers (dtd, out, ir, ltir,
suspended), start-share prior / k, off-night bonus. Not eligible: age curves, `age_yoy`,
`k_baseline`, dynasty weights, recommender thresholds (they are not knobs, so a candidate
cannot carry them).

**Replay** (`harness/refit.py`). Every matured weekly projection snapshot in the ledger (one
per ISO week, v2 inputs) becomes one row per player: the archived inputs rebuilt into a
`Player`, the history baseline, that day's positional means and the realized targets (FPG over
the next 28 days for players with >= 8 GP; points over the next 7 days).
`inseason_projection(obs, params)` calls `valuation.valuate.player_rates` (the live
`_rates_for` without reasons) with candidate params, so replay and valuation cannot drift;
`week_projection` adds availability, games, off-nights and the start share. With the
active params the replay reproduces the archived `fpg` (the refit prints the check). For this
the archive now also stores the header `position_means`, keeps zero-valued stats in the input
lines (`"zeros": true`) and a goalie `share` block (source, starts, team games); rows written
before 2026-09-29 are replayed approximately and counted as such.

**Objective and search.** Tier A groups minimise `(1 - w) * MAE_hist + w * MAE_live` with
`w = n_live / (n_live + 3000)`: MAE_hist over the ~10k historical in-season checkpoints
(`data/backtest`, rest-of-season FPG, loaded lazily and cached per scoring), MAE_live over the
28-day FPG of the replay table. Tier B groups minimise the live 7-day points MAE (no
historical analogue). Coordinate descent over a 9-point grid inside the step bounds, one group
at a time from the current values; the two groups with the largest training gain are combined.
The search never sees the two most recent matured weeks: they are the holdout.

**The gate** (`refit.gate`), all required:

- >= 400 matured 28-day live observations over >= 4 weekly snapshots (goalie knobs also >= 150
  goalie observations; Tier B also >= 400 7-day observations);
- per-cycle step bounds: k +-20%, recency +-0.05 each (and summing to 1), projection weight
  +-0.10, availability +-0.10, start-share prior +-0.05, start-share k +-20%, off-night bonus
  +-0.02;
- at most 2 parameter groups changed;
- holdout MAE at least 1% better and the player-clustered paired-bootstrap 90% CI of the
  improvement above 0 (per objective involved);
- historical MAE no more than 0.5% worse.

**Calendar.** No refit before 2026-11-02 (`--force` bypasses only this lock). Refit days are
every 14 days from 2026-11-02; `fm harness daily` runs the refit once on those days: from
2026-11-16, with `harness_auto_apply` on in `prefs.json` (default on), it applies a passing
Tier A candidate; otherwise it only stores a proposal. A manual `--apply` can change Tier A
from 2026-11-02 and Tier B from 2026-12-01.

**Champion / challenger.** After a promotion, `grade_week` shadow-scores the replaced version
every week on the snapshot whose 28-day window matured that week (snapshots from the promotion
on), recording both MAEs in the active version's `shadow.weeks`. If the shadow wins two weeks
running, the active version is rolled back to it automatically (changelog `auto-rollback`, a
`runs` row).

Before 2026-11-02 `fm harness refit --dry-run` prints "locked until 2026-11-02"; with
`--force` it reports how many matured observations are still missing.

## Known limits

- ESPN's activity feed has no IR or lineup messages; IR moves are detected from lineup slots.
- Fantrax trade rows are read as "the player moved to this row's team"; the giving team is
  inferred when exactly two teams are in the trade.
- Fantrax lineups can only be captured as the current roster at run time, so a week's lineup
  is known once `fm harness daily` has run on a day after the Monday lock.
- v1 recommendations (2026-09-28) have no predicted gain or subjects; IR targets are recovered
  from the title and that day's projections.
