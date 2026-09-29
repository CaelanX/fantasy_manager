# Trade scoring

`fm trades`, the Trades section of `fm advise`, the digest and the web Moves page all come from
`recommend.trades.recommend_trades`. This page explains how a proposal is scored, how to read
it, and how the acceptance model will be calibrated.

## The idea

Deals with massive upside for you are deals nobody accepts. A realistic trade is a small win:
small enough that it makes sense to the manager you send it to, big enough to be worth sending.
So every candidate is scored from both sides and ranked by **expected value**:

```
EV = my edge x p_accept
```

A +0.5 pts/game deal the other manager takes 70% of the time (EV 0.35) outranks a +1.5 deal
they take 20% of the time (EV 0.30).

## My edge

`my edge` = the change in my optimal starting lineup's season points per game (ΔMe, the exact
lineup solver) after the trade, including any drop I must make, IR moves and a free-agent pickup
when I free a roster spot. `predicted_gain` stays ΔMe in `lineup_fpg`, which is what the harness
grades.

Dynasty leagues add the long-term value change ΔDyn (dynasty values in, out, dropped, picked up,
minus a replacement-level asset for every extra roster spot the trade uses), normalised to
FPG-like units, weighted by the dynasty mode:

| mode | my edge | this-season floor |
|---|---|---|
| contend | ΔMe + 0.5 ΔDyn | ΔMe >= -0.3, else the deal is "future-only" (WIN_NOW_COST) |
| balanced | ΔMe + ΔDyn | ΔMe >= -1.0 |
| rebuild | 0.5 ΔMe + ΔDyn | none |

## Acceptance: the market view

The other manager does not see our model; they see consensus. `p_accept` therefore uses
**market value**, not our valuation.

1. **Market value** (`MarketPool`, 0-100): each player's percentile within the league's rostered
   players on each available signal, then a weighted mean:

   | signal | source | weight |
   |---|---|---|
   | ADP (lower = better) | ESPN | 2 |
   | projected FP/G | Fantrax's own projection | 2 |
   | % rostered | ESPN, Fantrax | 1 |
   | dynasty value | our dynasty model, dynasty leagues only | 2 |

   A signal is used only when at least 10 rostered players report it. Per-game signals rank
   goalies and skaters separately. A player with no market signal falls back to the percentile
   of our season FPG. Dynasty leagues price in our dynasty value because no provider publishes
   dynasty ADP and % rostered / season projections ignore age (a stand-in, marked as such).

2. **Worth**: `worth = 100 x (percentile / 100)^3`. Rank percentiles are too flat at the top to
   add up (in a 300-player pool the #2 and the #42 player are only ~13 percentile points apart),
   so deals are summed in worth points. In the middle of the pool 10 percentile points are about
   9 worth points; near the top, about 25.

3. **Perceived fairness** to them, in worth points (+ = in their favour):

   ```
   perceived = package(what they get) - package(what they give)
             - 3 per player they must drop to make room
             + 5 if an incoming player fills their weakest starting slot
             + 5 if the deal matches their trade block (a wanted position, or a player they offer)
   package  = best player's worth + 0.5 x each further player's worth
   ```

4. **Probability**: a logistic curve, capped at 0.9 (nobody accepts every offer):

   | perceived | p_accept |
   |---|---|
   | -20 | 0.10 |
   | -10 | 0.25 |
   | 0 (even) | 0.50 |
   | +10 | 0.75 |
   | +20 or more | 0.90 |

   `p = 1 / (1 + exp(-k (perceived - x0)))` with `x0 = 0`, `k = ln(3) / 10`.

5. **Multipliers**: 0 when the trade would leave their roster illegal
   (`recommend.base.roster_legal_after`: per-position maximums, maximum roster size). In dynasty
   leagues, once standings exist: x0.85 when a contender (top half by win %) gets no help this
   season, x0.85 when a rebuilder (bottom third) receives players at least 3 years older than
   the ones it sends.

These numbers are a **prior**, not a fit. See calibration below.

## Filters

A candidate is proposed only when:

* my edge > 0.3,
* p_accept >= 0.25,
* both rosters are legal after the trade,
* it does not force me to drop a protected young prospect (dynasty),
* in contend mode, it does not cost this season's lineup more than 0.3 pts/game.

The old fairness band on our own model value is gone as a filter. The model-value gap is still
shown (FAIR_PCT, "Model-value gap 8%") as information.

Candidates are 1-for-1, 2-for-1 and 1-for-2 over each side's top 12 players. Deals whose market
packages cannot reach p_accept 0.25 even with both bonuses, or that hand the other side more
than 30 worth points, are skipped before any lineup is solved.

## Diversity

At most 3 proposals per counterparty, 2 per player you give away and 2 per player you would
receive, so the list is not eight variations of "trade Quinn Hughes".

## Sweet spot

The model-versus-market arbitrage: deals the market calls fair (|perceived| <= 5 worth points)
while our model says you gain at least 0.4. They are listed separately (top 5), ranked by

```
sweet_spot_score = my edge - 0.05 x max(0, -perceived)
```

(how much the market thinks they lose, at 10 worth points ~ 0.5 FPG). These are the easiest
deals to propose: the other manager sees an even trade.

## Reading a proposal

Every trade carries these reasons (first in this order):

| code | what it says |
|---|---|
| MY_EDGE | "You gain +0.62 pts/game this season (dynasty +1.20; balanced edge +1.82)" |
| MARKET_VIEW | "Looks even to them by market value (ADP/rostered %); acceptance ~70%". Value: perceived worth points |
| THEIR_NEED | "X fills Team's weakest slot D (starter VORP -0.40)" |
| TRADE_BLOCK | "X is on Team's trade block" / "X matches Team's trade-block wants (C, D)" |
| ROSTER_CONSEQUENCE | "Roster: you drop Y; Team can pick up Z (FA)", or "straight 1-for-1, no other moves needed" |
| SWEET_SPOT | present on sweet-spot deals. Value: sweet_spot_score |
| P_ACCEPT | the probability (value), its raw logistic value (baseline) and the arithmetic, e.g. "EV = +0.62 x 0.70 = +0.43" |

The detail follows: DELTA_ME, DELTA_THEM (their lineup change by our model), VALUE_IN /
VALUE_OUT, FAIR_PCT, DYNASTY_DELTA, WIN_NOW_COST, IR_MOVE, POSITION_CAP, ROSTER_DROP, FA_FILL.

Market labels: "even" within 3 worth points; "slightly" up to 10; "clearly" beyond.

`Recommendation.score` is the EV (advise() later rescales scores by rank; the raw EV stays in
the RAW_SCORE reason). **Strength** (0-10) maps EV x confidence:

| EV | 0.15 | 0.35 | 0.6 | 1.0+ |
|---|---|---|---|---|
| strength | 3 | 6 | 8 | 10 |

Confidence is the players' games played this season, GP / (GP + k), floored at 0.25. Before the
season starts every player is at the floor, so strengths read about a quarter of their in-season
value (a +0.5 FPG deal at 70% shows about 1.7). They rise as games are played.

Where to see it: `fm trades` (proposal table, then the Sweet spot table, then contend mode's
future-only list), `fm trades --json` (`trades`, `sweet_spot`, `future_only`), the web Moves page
(each trade card shows acceptance and expected value; a "Sweet spot trades" section follows the
list), and the digest (trade lines show only the reader-facing reasons). Without a separate
sweet-spot list (advise, the web), up to half of the proposal slots are reserved for sweet-spot
deals.

## Calibration

The acceptance curve is a prior until real proposals exist. The harness already records them:
the matcher marks a trade episode `proposed` when my offer shows up in the provider's pending
trades (Fantrax) and `followed` when the trade completes (`rec_episodes.status`).

`recommend.trades.calibrate_acceptance(ledger, league=None)` returns the curve's parameters:

* fewer than 20 logged proposals (`status in ('proposed', 'followed')`): the prior, unchanged;
* 20 or more: a logistic regression of accepted (`followed`) on the MARKET_VIEW value logged with
  the episode's first rec, shrunk toward the prior (ridge), giving new `x0` and `k`.

TODO(harness): call it from the refit step and pass the result to `recommend_trades(...,
accept_params=...)`; count a proposal still pending when its window closes as a rejection. The
weights (market signals, worth convexity, depth weight, bonuses) are priors too and are the next
thing to fit once there are enough cases.

## Constants

All in `fantasy_manager/recommend/trades.py`:

| constant | value | meaning |
|---|---|---|
| MIN_GAIN_ME | 0.3 | my edge must exceed this |
| MIN_P_ACCEPT | 0.25 | plausibility floor |
| ACCEPT_X0, ACCEPT_K | 0, ln(3)/10 | logistic midpoint and slope |
| P_CEILING | 0.9 | acceptance cap |
| MARKET_WEIGHTS | adp 2, proj 2, owned 1, dyn 2 | market signal weights |
| MARKET_MIN_REF | 10 | rostered players needed for a signal |
| MARKET_CONVEXITY | 3 | worth = 100 x pct^3 |
| DEPTH_WEIGHT | 0.5 | a package's 2nd player |
| ROSTER_SPOT_PENALTY | 3 | per player they must drop |
| NEED_PTS, BLOCK_PTS | 5, 5 | weakest-slot fit, trade-block match |
| CONTENDER_FUTURE_MULT, REBUILDER_VETERAN_MULT | 0.85, 0.85 | dynasty standing modifiers |
| SWEET_MAX_PERCEIVED, SWEET_MIN_EDGE | 5, 0.4 | sweet-spot definition |
| MARKET_PTS_TO_FPG | 0.05 | sweet-spot penalty per worth point |
| MAX_PER_GIVEN, MAX_PER_RECEIVED, max_per_team | 2, 2, 3 | diversity caps |
| OVERPAY_CAP | 30 | skip candidates this far in their favour |
| CALIBRATION_MIN_N | 20 | proposals before the curve is refitted |
