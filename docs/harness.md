# The evaluation harness

The harness keeps score. Every day it records what the model recommended, what you actually did
and what then happened in the NHL. From that it grades recommendations and projections (the
"bar"), and every two weeks it may nudge a handful of valuation parameters, within tight limits.
The plan of record is `docs/harness-plan.md`.

## Daily use

One scheduled command does the capture (see `docs/scheduling.md`):

```
fm harness daily            # both leagues; --league espn|fantrax for one
```

Other commands:

| Command | What it does |
|---|---|
| `fm harness status` | The bar (accuracy, hit rates, trust labels) and what the ledger holds |
| `fm harness grade [--week YYYY-MM-DD]` | Grade matured outcomes and compute a week's bar (`daily` does this on Mondays) |
| `fm harness ledger` | Recent recommendation episodes and whether you acted on them |
| `fm harness params [--history]` | Active valuation params, refittable values and the version history |
| `fm harness refit --dry-run` | Fit and gate a candidate, write nothing (`--force` ignores the calendar lock only) |
| `fm harness refit [--apply]` | Store a passing candidate as a proposal (`--apply`: also activate it) |
| `fm harness rollback [--to vNNNN\|packaged]` | Undo the active params version (to its parent by default) |
| `fm harness auto-apply [on\|off]` | Show or set whether refit days may apply a passing candidate by themselves |
| `fm harness rebuild` | Rebuild the ledger's archive-derived tables from `data/archive` |

Every command takes `--json`. The dashboard's **Health** tab (`/health`) and `/api/health.json`
show the same information.

## What gets captured

Everything lives in `<FM_DATA_DIR>/harness.db` (sqlite). `fm harness daily` runs, per league:

1. **Archive** today's projections and recommendations (skipped if `fm backtest archive`
   already wrote today's files). Archive files store the inputs of every projection
   (season-to-date and recent lines, history baseline, provider projection, status, games next
   week, goalie start share) plus the params and code hashes, so a projection can be replayed
   later with different parameters.
2. **Ingest** the archive into the ledger. The same recommendation on consecutive days becomes
   one **episode**.
3. **Pull transactions** from the last 7 days (adds, drops, trades, pending Fantrax trades) for
   every team, and **lineups** (ESPN box scores for yesterday, a Fantrax roster snapshot).
4. **Pull yesterday's NHL results** for every player.
5. **Match** episodes to your moves: *followed* (you did exactly what it said), *partial* (part
   of it), *proposed* (a trade you offered), *expired* (you didn't; graded as *ignored*). Your
   moves that no recommendation explains are *your own moves* (`user_only`).

On Mondays it also grades; on refit days it runs the refit. A second run the same day changes
nothing. Provider problems become warnings, never a failed run for the other league.

Run it every day: the ESPN activity feed pages back 25 moves at a time and Fantrax keeps only the
last 50 transactions, so skipped days can lose moves for good.

## What the bar means

Graded weekly (as of Monday; only windows that have fully ended count):

- **Projection accuracy**, per pool (forwards, defense, goalies). Each Monday's projection is
  compared with the player's actual fantasy points per game over the next 28 days (players
  with 8+ games). Reported as MAE (mean absolute error, lower is better) next to four
  baselines: season-to-date FPG, the frozen preseason projection, the provider's projection
  and last season. The headline number is the **skill score**, 1 − MAE_fm / MAE_season-to-date:
  +10% means the model's errors are 10% smaller than just trusting season-to-date. It comes
  with a 95% confidence interval (bootstrap, resampling whole players).
- **Hit rate** per recommendation kind and per how you acted on it: a waiver or injury rec
  "hits" when the added player outscored the dropped one over the window (28 days; 7 for
  lineup and week-horizon recs). Shown with a 95% Wilson interval. Followed, ignored and
  your-own moves are reported separately, because what you chose to follow is not a random
  sample.
- **Calibration**: predicted gain vs realized gain by bin. A realized/predicted ratio near
  1.00 means the gains are sized right.
- **Your moves vs the model's**: for your own waiver moves, what they gained against the
  model's top pick of that day over the same window.
- **Trades** are too few to average; they are listed case by case.

## Trust labels

Every number carries one. Hidden numbers are never shown as results.

| Metric | hidden | provisional | reliable |
|---|---|---|---|
| Projection MAE / skill (per pool) | < 150 player-windows | 150+ | 400+ over 4+ weekly snapshots |
| Goalies | as above | at most provisional before January 1 | from January |
| Hit rate (per kind and origin) | < 20 graded | 20-49 | 50+ |
| Calibration | < 100 graded | | 100+ |
| Trades | always a case list | | |

The digest's one-line "Model: ..." headline and its 3-line **Model health** block (MAE trend
arrow, hit rate, params version) appear only once something is provisional or better.

## How auto-correction works, and its limits

Only a few in-season knobs can change: **Tier A** (may be applied automatically) are the
in-season shrinkage `k` (skaters, goalies), the recency weights, the projection weight and its
`k`. **Tier B** (proposals only until December 1, never automatic) are the availability
multipliers, the goalie start-share prior and `k`, and the off-night bonus. Age curves,
historical baselines, dynasty weights and recommender thresholds are never refit.

A refit replays every matured weekly snapshot with candidate values, searches within per-cycle
step bounds (k ±20%, recency ±0.05, projection weight ±0.10, availability ±0.10), and keeps at
most two parameter groups. The loss blends this season's error with ~10,000 historical
checkpoints, so a hot month cannot drag the model far. A candidate is promoted only if it
passes **the gate**:

- at least 400 matured observations over 4+ weekly snapshots (goalie knobs: 150 goalie observations);
- the two most recent weeks, held out from the search, improve by at least 1% with a 90%
  confidence interval above zero;
- historical error no more than 0.5% worse.

After a promotion the replaced version keeps being scored every week (champion/challenger). If
it wins two weeks running, the harness rolls back to it automatically.

Limits: nothing can be judged before week 2; skater projections become provisional around week
4 and reliable around week 8; goalies and trades can't really be judged this season; dynasty
value can't be judged at all. Refits correct small calibration drift, not a broken model.

## The calendar

| Date | What happens |
|---|---|
| until 2026-11-02 | Capture and grading only; refits are locked (preseason) |
| 2026-11-02, then every 14 days | Refit day: `fm harness daily` stores a passing candidate as a proposal |
| from 2026-11-16 | On refit days a passing Tier A candidate is applied automatically (if auto-apply is on) |
| from 2026-12-01 | Tier B candidates can be applied by hand (`fm harness refit --apply`) |

## Reading /health

The page shows one league (switch with ESPN / FANTRAX in the header).

1. **Headline card**: the model headline, or "Not yet judgeable" with what each metric still
   needs; the active params version, where it came from and its hash; the next refit day and
   whether auto-apply is on.
2. **Projection accuracy**: small charts per pool, one point per weekly snapshot. The bold line
   is the model, thin lines are the baselines (legend under each chart). The skill chart's
   zero line is season-to-date: above it, the model is better. Pools still hidden are listed
   under the charts with how many observations they need. Each chart has a **Data table**.
3. **Recommendation hit rate** per kind, per graded week, with the shaded 95% band. A wide band
   means few graded recs.
4. **Scorecard**: hit rates for followed, partly followed, ignored and your own moves, then your
   moves vs the model's.
5. **Calibration** table, once 100 recs are graded.
6. **Valuation parameters**: every version (newest first) with changed keys, holdout and
   historical MAE before → after, and status (`active`, `proposed`, `shadow`, `rolled_back`,
   `retired`). **Propose refit (dry run)** runs the refit without writing anything and shows the
   proposal inline (it can take several seconds; before November 2 tick "Ignore the calendar
   lock" to see how far the data is from the gate).
7. **Data capture**: episodes by kind and status, transactions, realized NHL days, unmatched
   player identities, the last daily run and any warnings.

## Rolling back

On `/health`, under Valuation parameters: pick the version (the active version's parent is
preselected, `packaged` is the original fit), tick the confirmation box and press **Rollback**.
From a terminal: `fm harness rollback` (to the parent) or `fm harness rollback --to v0002`. The
dashboard recalculates with the restored params on the next page load; a CLI rollback needs the
dashboard's **Refresh data**. Every version is kept, so a rollback can itself be undone.

## FAQ

**Why is everything hidden?** Because there isn't enough graded data yet. A 28-day window
needs 28 days to finish, and a hit rate over 5 recommendations says nothing. The headline card
lists how many more observations each metric needs.

**Why did the params change?** A refit day found a candidate that passed the gate, and auto-apply
was on. `/health` (or `fm harness params --history`) shows which keys changed, the before/after
errors and who applied it (`auto`, `manual`, `dashboard`, `auto-rollback`). The footer of every
dashboard page shows the active version.

**How do I turn auto-apply off?** `fm harness auto-apply off`. Refit days then only store
proposals; apply one with `fm harness refit --apply`. `fm harness auto-apply on` turns it back on.
To ignore harness versions entirely for one run, set `FM_PARAMS_OVERRIDE=0`.

**A move of mine shows as "your own move" but the model suggested it.** Matching is strict: a
waiver rec must be acted on within its window (last seen + 3 days; trades + 7). Moves made much
later count as your own.

## Known limits

- ESPN's activity feed has no IR or lineup messages; IR moves are detected from lineup slots.
- Fantrax lineups are known only from roster snapshots, so a week's lineup is captured once
  `fm harness daily` runs after the Monday lock.
- Recommendations archived on 2026-09-28 (archive v1) have no predicted gain.
- Players without an NHL id (see "unmatched ids" on `/health`) can't be graded; `fm sync --review` fixes most.
