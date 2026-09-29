# Self-correcting evaluation harness: plan of record

Written 2026-09-28. Goal: keep a running bar on model performance, grade every recommendation (and every move the
user actually made) against realized results, and auto-correct a bounded set of valuation parameters at intervals
without chasing noise.

## Findings that shape the design

1. `Recommendation.score` is a 1–10 rank; the real predicted gain lives only in reason codes with mixed units
   (`LINEUP_GAIN` week pts, `VORP_DELTA` season FPG, `DELTA_ME` lineup FPG). Add `predicted_gain`, `gain_units`,
   `horizon_days` to `Recommendation` and fill them in every recommender.
2. The archive (v1) stores outputs only. **Archive v2 must store inputs** (season-to-date, L30/L15/L7, history
   baseline, projection baseline, status, games_next7, start_share, params_hash, code_hash) or nothing can be refit.
3. Valuation constants are module-level; refactor `blend`/`adjust`/`schedule` to read through `params.py`
   accessors at call time so a promoted params version takes effect on `/refresh` and every CLI run.
4. Transaction feeds are short (ESPN 25/page, Fantrax last 50): pull daily or moves fall off the end.

## 1. Ledger (`<fm_data_dir>/harness.db`, `fantasy_manager/harness/ledger.py`)

Tables: `runs`, `recs`, `rec_players`, `rec_episodes`, `transactions`, `lineup_days`, `projections`,
`realized_daily`, `decisions`, `outcomes`, `metric_snapshots`, `param_versions`.

- `rec_key = sha1(league, kind, sorted(add), sorted(drop), counterparty)`; the same rec seen on consecutive days is
  one episode graded from `first_seen` (secondary grade from the acted-on date).
- Matching acted-on recs (`harness/match.py`): window `first_seen .. last_seen+3d` (+7d trades). Waiver: my ADD of
  an `in` cid (`followed` if the drop matches, else `partial`). Trade: shared give+get set. Lineup: `in` starting and
  `out` benched on the first lockable day (Fantrax: next Monday). Injury: player in IR within 2 days. Flags: a trade
  involving the player within 14 days. My transactions with no episode become `decisions(origin=user_only)`.
  Opponents' adds are logged as league-wide samples.
- Realized outcomes (`harness/realized.py`): nightly league-wide NHL pull for yesterday (raw stats, scored at grade
  time with each league's ScoringConfig), ESPN box scores and a Fantrax post-lock roster snapshot for `lineup_days`,
  and a provider-truth check (flag |provider pts − NHL-derived pts| > 5%).
- Windows: 7d, 28d, rest-of-season from the day after the rec. Waiver Δ = pts(in) − pts(out); 2-for-1 trades
  subtract the replacement FA's realized points; ESPN lineup Δ only over days both played; dynasty trades get a
  rest-of-season grade labelled partial.
- `harness/ingest.py` backfills from the JSON archive (v1 and v2), idempotent; JSON stays the source of truth and the
  db is rebuildable (`fm harness rebuild`).

## 2. Metrics: the bar (`harness/metrics.py`, reusing `backtest.evaluate`)

- Projection accuracy: one snapshot per ISO week (Monday). Predicted `fpg` vs realized FPG over the next 28 days
  (players with ≥ 8 GP); predicted `proj_week` vs realized 7-day points. Per pool (F/D/G) and league scoring. Split
  rate error from availability error. Baselines from archived snapshots: season-to-date FPG, frozen preseason fm,
  provider projection, last-season FPG. Headline: skill score = 1 − MAE_fm/MAE_to_date with a paired bootstrap CI
  clustered by player.
- Recommendation quality per kind and origin (followed / ignored / user_only): hit rate (Wilson CI), mean realized Δ,
  calibration by decile (terciles when n < 100). Flag grading: sell-high hits if next-28d FPG < L15 FPG at flag time.
- Counterfactual: "your moves vs the model's" for user-only moves.
- Trust labels in `metric_snapshots.trust`: projection MAE provisional at ≥ 150 player-windows/pool, reliable at
  ≥ 400 and ≥ 4 non-overlapping weeks; goalies never better than provisional before January; hit rate hidden < 20,
  provisional 20–49, reliable ≥ 50; calibration needs ≥ 100; trades shown as a case list, never an aggregate.

## 3. Auto-correction (`harness/refit.py`, `harness/params_store.py`)

- Layering: `params.load_params()` deep-merges packaged `fitted_params.json` with
  `<fm_data_dir>/harness/params/active.json` → `vNNNN.json`; `FM_PARAMS_OVERRIDE=0` disables; `params.reload()`
  on `/refresh`; `source()` reports provenance.
- Tier A (auto-apply allowed): `k_inseason` {skater, goalie}, `recency_weights`, `projection_weight`, `k_projection`.
  Tier B (proposals only until December): availability multipliers, start-share prior/k, off-night bonus.
  Not eligible: age curves, `age_yoy`, `k_baseline` (sample too small); dynasty mode weights/discount/market weight
  (preferences); recommender thresholds (reported, changed manually).
- Fitting: replay table from v2 archive inputs + forward 28d realized; pure `inseason_projection(o, params)`;
  pooled loss = (1−w)·MAE_hist + w·MAE_live with w = n_live/(n_live+3000); coordinate-descent grid within step
  bounds; forward-chained validation holding out the two most recent matured weeks.
- Guardrails (`refit.gate()`): ≥ 400 matured live obs and ≥ 4 weekly snapshots (goalies ≥ 150); step bounds per
  cycle k ±20%, recency ±0.05 (sum to 1), projection weight ±0.10, availability ±0.10; at most 2 parameter groups
  per cycle; promotion needs holdout MAE ≥ 1% better with bootstrap 90% CI > 0 AND historical MAE no worse than
  0.5%; champion/challenger shadow scoring with automatic rollback after 2 straight losing weeks; every version kept
  with parent, hash, metrics, changelog.
- Cadence: ledger daily; grading Monday after the Fantrax lock; refit every two weeks; preseason locked until
  2026-11-02; Tier A auto-apply from 2026-11-16 when `prefs.harness_auto_apply` (default on).

## 4. Surfaces

- CLI `fm harness`: `status`, `daily` (cadence-aware, idempotent), `grade [--week]`, `refit [--dry-run|--apply]
  [--only ...]`, `rollback [--to vNNNN]`, `ledger [...]`, `params [--history]`, `rebuild`.
- Dashboard `/health` + `/api/health.json`: inline-SVG sparklines (rolling MAE, skill score by pool, weekly hit rate
  with CI band), calibration table, followed/ignored/user-only scorecard, params changelog with rollback (POST).
- Digest: one headline line, hidden until trustworthy. Scheduling: add `fm harness daily` after `fm backtest archive`.

## 5. Honesty and failure modes

Nothing to say before week 2; skater projection MAE provisional ~week 4; reliable ~week 8; goalies and trades can't
be judged this season; dynasty value can't be judged at all. Mitigations: pooled historical loss against hot-streak
chasing; rate vs availability error split against injury noise; grade lineup recs only on lockable days; report
followed vs ignored separately against selection bias; one snapshot per week and player-clustered bootstrap against
autocorrelation; ≤ 2 groups per cycle against multiple testing; league-wide bias flagged (|bias| > 0.03 for 3 weeks)
but never auto-corrected; unmatched identities counted on `/health`.

## 6. Build order

- **M1 capture (before first games):** archive v2 inputs + hashes; `predicted_gain`/`gain_units`/`horizon_days`
  on recommendations; `providers.espn.activity(since)` (paged `recent_activity`) and `box_scores(scoring_period)`;
  `providers.fantrax.activity()` with `when` parsing; `harness/{ledger,ingest,realized,match}.py`; `cli_harness.py`
  with `daily`, `ledger`, `rebuild`; tests for matching scenarios, episode dedup, v1 ingest.
- **M2 grading:** `harness/metrics.py`, `grade`, `status`, trust labels, digest line; tests on synthetic windows.
- **M3 auto-correction:** params accessor refactor and override layering; `params_store`, `refit`, `rollback`;
  property tests for guardrails (hypothesis).
- **M4 surfaces:** `/health` page, docs/harness.md, scheduling update, shadow scoring auto-rollback.
