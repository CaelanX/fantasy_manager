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

### In points: per week and rest of season

"+0.66 per game" is the change in the sum of my starters' per-game values, which is hard to feel.
Every trade therefore also carries it in points (`recommend.base.game_rate`):

```
pts/week          = ΔMe x my starters' average NHL games per week      (GAIN_WEEK)
pts rest of season = ΔMe x my starters' average NHL games left          (GAIN_SEASON)
```

Both come from the NHL schedule on the context (`ctx.schedule`): for each of my current starters
(starting slots; every non-IR player when no starters are flagged) whose NHL team is on the
schedule, the team's games from today (or opening night, preseason) to the last regular-season
game, averaged; games per week = that / the weeks left (at least 1). Without a schedule: 3.4
games per week and 82 x the share of the season still to play (season = `ctx.season_start`, else
Oct 7, through Apr 16). After the last game both are 0.

`fm trades` shows the "You gain" column as `+0.66/g · +2.3/wk · +55/season` (plus `dyn +x.xx` in
dynasty leagues), the web trade card adds "you gain +2.3 pts/week · +55 pts rest of season", and
the MY_EDGE text reads "You gain +0.66 pts/game this season (+2.3 pts/week, +55 rest of season)".
`predicted_gain` is unchanged (ΔMe in lineup FPG).

### Dynasty

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
| MY_EDGE | "You gain +0.62 pts/game this season (+2.1 pts/week, +51 rest of season) (dynasty +1.20; balanced edge +1.82)" |
| MARKET_VIEW | "Looks even to them by market value (ADP/rostered %); acceptance ~70%". Value: perceived worth points |
| THEIR_NEED | "X fills Team's weakest slot D (starter VORP -0.40)" |
| TRADE_BLOCK | "X is on Team's trade block" / "X matches Team's trade-block wants (C, D)" |
| ROSTER_CONSEQUENCE | "Roster: you drop Y; Team can pick up Z (FA)", or "straight 1-for-1, no other moves needed" |
| SWEET_SPOT | present on sweet-spot deals. Value: sweet_spot_score |
| P_ACCEPT | the probability (value), its raw logistic value (baseline) and the arithmetic, e.g. "EV = +0.62 x 0.70 = +0.43" |

Then GAIN_WEEK ("+2.1 pts/week (+0.62/game x 3.4 games/week, your starters' NHL schedule)",
value: points, baseline: games per week) and GAIN_SEASON ("+51 pts rest of season (+0.62/game x
83 games left)", value: points, baseline: games left). Exploit deals add EXPLOIT (third, after
MARKET_VIEW) and, when my free-agent pickup uses a move under a move limit, MOVE_BUDGET.

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

Where to see it: `fm trades` (proposal table, then the Sweet spot table, the Exploit section, then
contend mode's future-only list), `fm trades --json` (`trades`, `sweet_spot`, `exploits`, `future_only`), the web Moves page
(each trade card shows acceptance and expected value; a "Sweet spot trades" section follows the
list), and the digest (trade lines show only the reader-facing reasons). Without a separate
sweet-spot list (advise, the web), up to half of the proposal slots are reserved for sweet-spot
deals.

## Exploits: teams under roster pressure

A manager who has to make a move is easier to deal with. `recommend.trades.exploit_opportunities(ctx,
values, dynasty_values=None, limit=8)` looks for opponents under roster pressure
(`team_pressures`):

| pressure | detected when | a deal relieves it when |
|---|---|---|
| position cap | a position is over its league maximum, or at it with an injured player there ("3 goalies at the G limit 3 with X injured": they cannot add a healthy one) | more players at that position leave them than arrive |
| IR logjam | more injured players (out / IR / LTIR) than IR slots | more injured players leave them than arrive |
| goalie shortage | fewer than 2 healthy goalies with games in the next 7 days (only with a schedule loaded and games that week) | I send more healthy goalies with games this week than I take |
| weak slot | their weakest starting slot's starter VORP < -1 (or the slot is empty); the text adds the league median of that slot's weakest starter | I send a player eligible there with a higher VORP |
| 0 moves left | **not detected**: providers only expose my own transaction counter (`ctx.moves_used_this_period`), not other teams' | - |

For each pressured team the candidates are 1-for-1 over my whole active roster (a weaker piece may
do) x their top 12 plus the pressured players, 2-for-1 over my top 12, and 1-for-2 taking at least
one pressured player; only deals that relieve a pressure are scored. Scoring is the normal one
(my edge, market view, p_accept, EV) with **PRESSURE_PTS = 8** worth points added to their
perceived fairness ("+8 relieves their roster pressure" in P_ACCEPT; on top of the weakest-slot
and trade-block bonuses). The normal filters apply: my edge > 0.3, p_accept >= 0.25, both rosters
legal after the trade (position maximums, roster size; the same drops and IR moves), no forced
protected-prospect drop, the dynasty mode's this-season floor. A deal that needs my free-agent
pickup is skipped when I have no moves left (`base.moves_left`), and says which move it uses when
a limit applies (MOVE_BUDGET).

At most one deal per pressure kind and 2 per team, 2 per player I give, `limit` (8) overall, by EV.
Each carries EXPLOIT: "Exploit: Team X has 3 goalies at the G limit 3 with Y injured (no room to
add a healthy one)" (value PRESSURE_PTS).

Where: `fm trades` prints an "Exploit" section (one "Exploit: <team> has <pressure>" line per deal,
then the table; `--exploits N` caps it, 0 skips it; `--json` adds `exploits`), and the web Moves
page adds an "Exploit trades" section (all / trade filters).

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
| PRESSURE_PTS | 8 | the deal relieves their roster pressure (exploits) |
| PRESSURE_WEAK_VORP, MIN_HEALTHY_GOALIES | -1, 2 | weak-slot and goalie-shortage pressure |
| EXPLOIT_N, EXPLOIT_PER_TEAM | 8, 2 | exploit list size, deals per pressured team |
| DEFAULT_GAMES_PER_WEEK, SEASON_GAMES (`recommend/base.py`) | 3.4, 82 | gain units without a schedule |
| CONTENDER_FUTURE_MULT, REBUILDER_VETERAN_MULT | 0.85, 0.85 | dynasty standing modifiers |
| SWEET_MAX_PERCEIVED, SWEET_MIN_EDGE | 5, 0.4 | sweet-spot definition |
| MARKET_PTS_TO_FPG | 0.05 | sweet-spot penalty per worth point |
| MAX_PER_GIVEN, MAX_PER_RECEIVED, max_per_team | 2, 2, 3 | diversity caps |
| OVERPAY_CAP | 30 | skip candidates this far in their favour |
| CALIBRATION_MIN_N | 20 | proposals before the curve is refitted |
