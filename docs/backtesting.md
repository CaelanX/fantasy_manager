# Backtesting the projection model

`fm backtest` answers "is our projection model any good?" with numbers from free NHL history,
fits the valuation constants from data, and archives the commercial (ESPN / Fantrax)
projections so they can be graded at season end.

Everything lives under `<FM_DATA_DIR>/backtest/` (default `data/backtest/`) and
`<FM_DATA_DIR>/archive/`.

## Commands

| Command | What it does |
|---|---|
| `fm backtest data [--from-season 20102011] [--to-season 20252026] [--inseason 20212022-20252026] [--force]` | Build or refresh the per-player-season table and the in-season checkpoint windows. Incremental; stored seasons are skipped. |
| `fm backtest run [--scoring espn\|fantrax\|default\|file.json] [--models naive,marcel,fm_current,fm_fitted,fm_multi] [--seasons 20162017-20252026] [--no-inseason]` | Evaluate the models and write `results-YYYY-MM-DD.json` and `report-YYYY-MM-DD.md` (non-ESPN scoring adds a `-<scoring>` suffix). |
| `fm backtest fit [--scoring espn] [--boot 200]` | Fit shrinkage k, in-season k and recency weights, and age curves, then write `fitted_params.json` to the data dir and print the command that promotes it to the app. |
| `fm backtest archive [--league espn\|fantrax\|all] [--no-recs]` | Snapshot provider projections, our blended rates and the recommendations to `data/archive/`. Safe to run daily. |
| `fm backtest grade --season 20262027 [--snapshot first\|last\|YYYY-MM-DD] [--scoring espn]` | After a season ends, score the archived provider projections against ours using actual FPG. |
| `fm backtest report` | Print the latest report. |

Typical first run: `fm backtest data`, then `fm backtest fit`, then `fm backtest run`. `run`
includes the fitted-parameter section when `fitted_params.json` exists.

## How the app uses the fit

The app reads `fantasy_manager/valuation/fitted_params.json`, a copy of the ESPN-scoring
`fitted_params.json` shipped as package data, through `valuation/params.py` (typed accessors with
hard-coded fallbacks, so a missing or broken file never breaks valuation). `fm backtest fit`
never overwrites it: it writes to `<FM_DATA_DIR>/backtest/` and prints the `copy` command to
promote the new fit. After promoting, run the tests and `fm backtest run`.

What the app takes from the file:

| Constant | Source | Used for |
|---|---|---|
| `blend.K_BASELINE` {F, D, G} | `preseason.k_multi` (8 / 14 / 120) | 3-season history toward the positional mean |
| `blend.K_INSEASON` {skater, goalie} | `inseason.k_skater` / `k_goalie` (25 / 40) | season-to-date toward the baseline, and waiver confidence GP/(GP+k) |
| `blend.RECENCY_WEIGHTS` | `inseason.recency_weights` (0.85 / 0.15 / 0 / 0) | recent-form blend (still applied, so `--deep` matters a little) |
| age factor | `fm_fitted.age_yoy` (F, D only) | the history baseline, from last season's age to this one |
| `dynasty.PRODUCTION_CURVES` F, D | `dynasty_age_curves` | dynasty trajectory (goalies stay hand-set) |

`blend.K_PROJECTION` (20 skaters / 12 goalies, the old constants) is not fitted: it shrinks a
league projection toward the positional mean, and there are no historical projections to fit it
on. The baseline (`valuate.baseline_rates`) is:

- no league projection: the 3-season history (`blend.multi_season_baseline`, the same function
  `fm_multi` calls) times the age factor;
- league projection (ESPN or Fantrax `projected`): 0.6 x projection + 0.4 x history once the
  player has 40+ NHL GP over the three prior seasons, the projection alone with no NHL history
  (rookies), linear in between.

Seasons N-2 and N-3 come from `providers.enrich` (`prior2` / `prior3` lines): two extra
league-wide pulls of the NHL stats reports, cached 30 days.

## Data

- **Season table** (`player_seasons.json`): one row per player per regular season with G, A,
  PTS, PPG/PPA/PPP, SHG/SHA/SHP, SOG, HIT, BLK, PIM, FOW/FOL, ENG and GP. Goalie rows have
  GS, W, L, OTL, GA, SA, SV, SO, G, A, PIM. Each row also has the age on Oct 1 and the position
  group (F/D/G). Each season needs six stats-API reports (skater summary, realtime, faceoffwins
  and bios; goalie summary and bios). The `bios` report carries birth dates, so no per-player
  roster or landing calls are needed. Hits and blocks go back to at least 2009-10.
- **In-season windows** (`windows.json`): for Nov 1, Dec 1 and Jan 1 of each season, the same
  reports filtered by `gameDate` produce these windows for every player:
  - season-to-date
  - last 30, 15 and 7 days
  - rest of season

  This takes 15 requests per checkpoint. The per-player game-log endpoint would have needed about
  400 requests per season and doesn't include hits or blocks.
- All requests go through `HttpCache`. Finished seasons are cached for 30 days and the current
  season for 12 hours. The first full build (16 seasons plus 15 checkpoints) made 317 sequential
  requests in about 90 seconds. Re-runs make no requests.
- Storage is plain JSON (about 5 MB and 11 MB) because pyarrow and pandas aren't installed.

## Scoring presets

`espn` and `fantrax` are the live leagues' point values, captured from
`fm settings --league X --json` and stored in `tests/fixtures/backtest/scoring_*.json`. `default`
is a generic points league. Some stats aren't in the NHL season data and contribute nothing:
ESPN `HAT` and `DEF`, and Fantrax `FT` (fights).

## Models

Each model projects per-game rates for season N from seasons before N only.

| model | description |
|---|---|
| `naive` | Last season's rates. |
| `marcel` | Seasons N-1, N-2 and N-3 weighted 5/4/3 by GP, regressed toward the positional mean with k = 30 games. Classic Marcel age adjustment around a peak of 27: +0.6% per year younger, -0.3% per year older. |
| `fm_current` | The app's preseason path when there's no league projection. It calls `valuate.baseline_rates` with the app's `positional_means` on seasons N-1..N-3 (`prior` / `prior2` / `prior3`) and the age on Oct 1: the 3-season history with the packaged fitted k and age factor. Those constants were fitted on all seasons, so `fm_current` is slightly in-sample. |
| `fm_fitted` | Last season only, shrunk with a fitted k per group, times a year-over-year age factor. k and the age factor are re-fitted before each target season on earlier seasons only. |
| `fm_multi` | The 5/4/3 GP-weighted seasons, shrunk with a fitted k per group, times the fitted age factor, re-fitted before each target season (rolling origin). Its rates come from `valuation.blend.multi_season_baseline`, the app's own function, so the two cannot drift. |

**In-season checkpoints.** Each checkpoint compares these projections of rest-of-season FPG:

- `preseason`: the baseline alone (the app's: seasons N-1..N-3, fitted k, age factor)
- `to_date`: season-to-date FPG
- `fm_shrink_only`: the app's shrinkage with no recency blend
- `fm_current`: the app's `shrink_toward` plus `blend_recency`, run on real StatLines
- `fm_fitted`: fitted k and recency weights, leave-one-season-out

## Metrics

The preseason population is every player with at least 20 GP in both N-1 and N. The in-season
population is players with at least 1 GP before the checkpoint and at least 10 GP after it.

Metrics are computed per season and model, with skaters and goalies scored separately:

- MAE and RMSE of FPG
- bias (projected minus actual)
- Spearman rank correlation
- top-50 and top-100 precision: the share of the projected top N who finished in the actual top N
  (skaters only)

## Fitted parameters (`fitted_params.json`)

- `preseason.k` and `preseason.k_multi`: shrinkage k per group from a grid search that minimizes MAE.
- `inseason`: k for season-to-date toward the baseline, plus recency weights for season / L30 /
  L15 / L7. These come from a coordinate-descent grid search. The shortfall rule is the same as
  `blend.recency_weights`.
- `age_curve`: a delta-method curve per group (F, D, G). For players with at least 20 GP in two
  consecutive seasons:
  1. Take the median of `FPG(next) / base`, pooled over age ±1 year.
  2. Chain the medians year over year and normalize so the peak is 1.0.
  3. Compute 90% intervals with a player-clustered bootstrap.

  `base` is last season's FPG shrunk with the fitted k. Against raw last-season FPG, the ratios
  carry a regression-to-the-mean drift at every age: median 0.95 for goalies and 0.98 for
  forwards. Chaining turns that drift into a false decline. Ages with fewer than 25 pooled pairs
  stay flat.
- `dynasty_age_curves`: `[[age, level], ...]` in the shape of `valuation.dynasty.AGE_CURVES`.
  `level(age + y) / level(age)` is the expected trajectory.
- `fm_fitted`: the `FittedParams` dict (`k`, `k_multi`, `age_yoy`) that `models.FittedParams.from_dict` reads.

## Archive

`archive_projections(ctx, data_dir, values)` writes
`archive/projections-<provider>-YYYY-MM-DD.json`. It records every player's provider `projected`
StatLine, pct_owned, status, ids (including the NHL id), and our blended per-game rates.
`archive_recommendations` writes `archive/recs-<provider>-YYYY-MM-DD.json`. Each day has one file
per provider: a re-run that day replaces it, and identical content isn't rewritten.

`score_archive(season_actuals, data_dir)` takes the first snapshot of the season (preseason) by
default. It matches players by NHL id, then by unique normalized name. It then scores both the
provider projection and ours against actual FPG, on the players that both cover.

## Caveats

- There are no historical ESPN or Fantrax projections, so `fm_current` measures the app's
  no-projection path. Commercial projections can only be graded from the archive, starting with
  2026-27.
- Rookies, and players with fewer than 20 GP last season, aren't in the preseason population.
  The fitted k therefore comes from regulars; the app applies it to small samples too (a 5-GP
  defenseman keeps 5 / (5 + 14) = 26% of his own rate), which is an extrapolation.
- The age curve only includes players who stayed in the league (survivor bias), so the decline
  at 33+ is if anything understated. The ends of the goalie curve are based on very few players.
- Goalie FPG per game played is noisy. Every model has a rank correlation around 0.2 to 0.25.
