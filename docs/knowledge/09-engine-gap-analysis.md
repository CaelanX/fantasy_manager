# 09 - Engine gap analysis: the research compared with what the program does

Written 2026-11-10 from the code on branch `finish-player-tools` (working tree, uncommitted changes included) and from the built binary `target/release/fm.exe`.

## What this is

Files 01 to 08 in this folder collect published fantasy-sports knowledge. Their authors mostly did not read the program, so their statements about what the engine does are guesses. This file checks those guesses against the Rust code, states what the engine should be maximising in the owner's two head-to-head points leagues (ESPN 14-team redraft, Fantrax 12-team dynasty), lists where the code already agrees with the research, and ranks the places where it does not. Nothing was changed in the code. The output is a prioritised work list.

## How to read it

- **FPG** means fantasy points per game under one league's scoring. **Replacement level** is the FPG of the best freely available player at a position. **VORP** (value over replacement player) is a player's FPG minus that level. **Start share** is the fraction of his team's games a goalie starts. **k** is a shrinkage constant: with n games observed, the observed rate gets weight n / (n + k) and the prior gets the rest.
- Every statement about the code carries a `file:line` that was read for this document. Paths are relative to the repository root.
- Confidence labels: **confirmed in code** (the lines were read and say this), **confirmed by CLI** (the binary's output shows it), **likely** (follows from code that was read, but the effect was not measured), **unverified** (not checked).
- Gap categories: **(a)** bug, the code does something other than it intends; **(b)** a modelling choice the research contradicts; **(c)** something the research recommends that is missing; **(d)** a research claim about the engine that the code shows to be wrong.
- Sizes: small (hours, one file), medium (a day or two, a few files and tests), large (a new model or a new simulation).
- Numbers quoted from the binary are from 2026-11-10 and will drift.

## What the engine should optimise

This is the definition of "how to win" for these two leagues, taken from files 01 to 08.

1. **Season.** Maximise the probability of winning the title, which is P(make the playoffs) times P(win each knockout round). Weekly wins matter only through that chain, and a win is worth most to a team on the playoff bubble ([08 section 1](08-h2h-objectives-and-game-theory.md#1-the-season-objective-weekly-wins-playoff-qualification-championship), [08 section 5](08-h2h-objectives-and-game-theory.md#5-scarce-resources-across-the-season)). In the dynasty league the objective also includes future seasons ([07 section 2](07-dynasty-and-keeper.md#2-valuing-players-over-several-seasons)).
2. **Week.** Maximise the probability of winning the matchup. That is almost the same as maximising expected points. Variance only matters when the matchup is lopsided or nearly over, and it never justifies giving up a clearly better mean ([08 section 2](08-h2h-objectives-and-game-theory.md#2-the-weekly-objective-win-probability-not-expected-points), [01 section 3](01-fantasy-fundamentals.md#3-what-winning-a-head-to-head-league-requires)).
3. **Per-game value.** Project each scored statistic as opportunity (games, ice time, power-play share, goalie start share) times a per-opportunity rate. Update opportunity quickly, regress rates slowly, and regress percentages (shooting, on-ice shooting, save percentage) much harder than volumes (shots, hits, blocks) ([04 section 1](04-projection-and-regression.md#1-regression-to-the-mean-and-stabilisation), [04 section 6](04-projection-and-regression.md#6-in-season-updating), [03 section 1](03-hockey-player-value-drivers.md#1-skater-opportunity)).
4. **Replacement level.** Value is points over the alternative, per position and per league. In season the alternative is the real free-agent pool, and for one team it is the player who would actually be displaced ([01 section 2](01-fantasy-fundamentals.md#2-core-valuation-ideas), [06 section 3](06-trade-strategy.md#3-positional-need-and-lineup-fit)).
5. **Week value and lineup.** With daily lineups and no games cap, points come only from games that sit in an active slot. A player is worth his rate times the games he would actually start on this roster. A goalie start is an expected-value decision that can be negative on ESPN (break-even save percentage about .882) and is almost always positive on Fantrax (about .800) ([05 section 1](05-in-season-management.md#1-daily-lineup-setting), [03 section 7](03-hockey-player-value-drivers.md#7-how-our-scoring-shifts-value)).
6. **Weekly moves.** Two adds per matchup (ESPN) or three per week (Fantrax) are an inventory. Each move is judged by the started points it adds, net of the dropped player's future value, with a higher bar for the last move ([05 section 9](05-in-season-management.md#9-how-a-low-move-limit-changes-all-of-this)).
7. **Trades.** Judge the end-state rosters of both sides: the free agent who fills a freed spot, the player who must be dropped, and the games that can actually be started. Report closeness, not a winner, and expect a "fair" offer to be declined more often than accepted ([06 section 1](06-trade-strategy.md#1-how-experts-value-trades), [06 section 5](06-trade-strategy.md#5-the-other-side-of-the-table), [06 section 10](06-trade-strategy.md#10-what-a-good-automated-recommendation-looks-like)).
8. **Dynasty.** Value is a discounted sum of future value over replacement, with a flat age plateau from about 23 to 29, no youth bonus for goalies, a charge for any roster spot that produces nothing, and a contend-or-rebuild choice driven by playoff odds ([07 section 3](07-dynasty-and-keeper.md#3-aging-curves-in-hockey), [07 section 6](07-dynasty-and-keeper.md#6-contend-or-rebuild), [07 section 8](07-dynasty-and-keeper.md#8-small-rosters-and-the-cost-of-holding-prospects)).
9. **Grading.** Judge decisions by predicted probabilities and expected gains recorded beforehand, not by single outcomes ([08 section 8](08-h2h-objectives-and-game-theory.md#8-judging-decisions-independent-of-outcomes)).

## Where the engine already matches the research

| Principle | Where in the code | Evidence |
|---|---|---|
| Separate valuation per league from shared inputs | `crates/fm-engine/src/valuation/valuate.rs:1248-1317` | `valuate_league` takes the league's own scoring system. CLI replacement levels differ: ESPN F 1.60, D 1.60, G 1.08; Fantrax F 4.45, D 3.64. |
| Replacement level is the real free-agent pool, per position | `crates/fm-engine/src/valuation/replacement.rs:87-111`, `valuate.rs:834-855` | Mean of the top 3 free agents eligible at each slot, recomputed on every run; falls back to the rostered 10th percentile when a slot has no free agent (`replacement.rs:147-183`). |
| No positional scarcity bonus | `replacement.rs:118-125` | VORP is value minus the slot's replacement level and nothing else. A search for "scarcity" in `valuation/` and `recommend/` finds only the move-budget threshold. |
| Marcel-style history: three seasons weighted 5/4/3, shrunk to the positional mean, aged | `crates/fm-engine/src/valuation/blend.rs:28`, `blend.rs:179-193`, `valuate.rs:151-175` | Baseline k is 8 (F), 14 (D), 120 (G) games (`params.rs:30`). |
| Recent form gets little weight | `crates/fm-engine/src/valuation/params.rs:32` | Weights 0.85 season, 0.15 last 30 days, 0 for last 15 and last 7. |
| Goals regressed toward expected goals in season | `crates/fm-engine/src/valuation/regression.rs:16`, `regression.rs:196-209`, `valuate.rs:301-304` | k = 15 games toward individual expected goals. CLI: Draisaitl 0.95 goals per game against 0.61 expected is projected at 0.80. |
| Assist luck from on-ice shooting percentage is removed | `regression.rs:25-27`, `valuate.rs:709-767` | 35% of the luck times a confidence factor, capped at 15% of FPG. |
| Goalie save talent regressed with about 3,000 shots | `regression.rs:29-32`, `regression.rs:288-326` | Three seasons weighted 1 / 0.8 / 0.6, k = 3,000 shots. See gap D2. |
| Goalie start share is a separate input, measured from actual starts | `valuate.rs:991-1104`, `crates/fm-engine/src/valuation/schedule.rs:110-148` | Ledger starts shrunk toward the projected share with k = 10 team games. |
| A starter is not projected for both halves of a back-to-back | `schedule.rs:44`, `schedule.rs:268-298` | P(number one starts the second night) = 0.35 until 5 of the team's back-to-backs are seen. |
| An absent goalie's starts go to his partner, capped | `schedule.rs:331`, `schedule.rs:350-386` | Cap 0.90. The research suggests about 0.75 ([03 implication 14](03-hockey-player-value-drivers.md#implications-for-the-engine)). |
| Confirmed starters override the share for tonight | `crates/fm-engine/src/recommend/lineup.rs:1015-1024`, `lineup.rs:515-539` | Confirmed 1.0 / 0.0, likely 0.8 / 0.2. |
| A this-matchup pickup is valued by the started games it adds on this roster | `crates/fm-engine/src/recommend/waivers.rs:253-291`, `waivers.rs:558-566` | The best lineup is solved for each remaining day with and without the add. CLI `fm claims -l espn`: "+3.2 pts over the rest of this matchup", with the days he would start. |
| Moves are an inventory with a rising bar | `crates/fm-engine/src/recommend/base.rs:560-565`, `base.rs:616-624` | Gain must exceed 0.3 FPG with 4 or more moves left, 0.6 with 2 or 3, 1.0 for the last one unless it replaces an injured player. |
| Recently added players are not churned | `base.rs:567-571`, `waivers.rs:395-427` | A player added in the last 7 days is dropped only for +1.5 FPG or when he is hurt. |
| Hold or cut an injured player by slot value | `crates/fm-engine/src/recommend/injuries.rs:321-420` | Hold = his FPG times games after return; cut = replacement FPG times all remaining games; an IR slot makes holding free. |
| Trades are judged on end-state rosters | `crates/fm-engine/src/recommend/trades/engine.rs:969-1039`, `engine.rs:780-857`, `engine.rs:911-941` | Forced drops, IR moves, the best free agent for a freed spot, and a baseline that already includes that pickup. |
| A "too close to call" trade verdict exists | `crates/fm-engine/src/recommend/trades/offer.rs:210-212` | Edge between -0.3 and +0.1 returns "counter". See gap B3 for its limits. |
| Dynasty weights and age plateau | `crates/fm-engine/src/valuation/dynasty.rs:28-35`, `params.rs:44-51` | Contend 1 / 0.65 / 0.45, balanced 1 / 0.8 / 0.64, rebuild 0.7 / 0.9 / 0.9, plus a terminal term. The packaged forward curve is 0.98 at 23, 1.00 at 25, 0.90 at 29. |
| Contend or rebuild comes from simulated playoff odds, not roster age | `crates/fm-engine/src/analysis/outlook.rs:192-253`, `outlook.rs:257-290` | Contend at 55% or more, rebuild at 15% or less. The playoff-team count is read from the league (`outlook.rs:172-188`). |
| Playoff-week planning is roster-aware | `crates/fm-engine/src/analysis/playoffs.rs:307` | CLI `fm playoffs -l espn` lists each player's games and starts in periods 25 to 27. |
| Decisions are logged with predicted gains and graded later | `crates/fm-harness/src/outcomes.rs:589-729`, `crates/fm-harness/src/metrics.rs:712-753`, `crates/fm-harness/src/matchup_calib.rs:32-39` | Hit rates with Wilson intervals, predicted against realised gain, and Brier score and log loss for the matchup win probability. |

## Gaps and suspected errors

Ordered by expected effect on decisions. One precondition comes first because it changes how every Fantrax number should be read.

**Precondition: is the Fantrax league "keep everyone" or "keep three"?** The Fantrax Rules page says "Keeper league type: Dynasty" (CLI `fm settings -l fantrax`). The program is configured with `FANTRAX_KEEPERS`, read at `crates/fm-providers/src/snapshot.rs:43` and passed on at `crates/fm-providers/src/fantrax.rs:2975`, and `fm outlook -l fantrax` prints "next seasons count only each team's top 3 keepers (everyone else is redrafted)". In that mode a player outside his team's top three has no future value at all (`dynasty.rs:543-667`); CLI shows Byfield, age 24, with a dynasty value of 3.70, equal to his FPG. File 07 assumes every player is kept. One of the two is wrong and only the owner can say which.

### B1. Rest-of-season value counts only the starting lineup; the bench, the third goalie and the schedule are worth zero

- **Category:** (b). **Confidence:** confirmed in code and by CLI.
- **Research:** in a daily-lineup league value is rate times startable games, including bench players who start when starters are idle ([01 implication 4](01-fantasy-fundamentals.md#implications-for-the-engine), [06 implication 4](06-trade-strategy.md#implications-for-the-engine)).
- **Code:** a trade's gain for me is the change in the sum of my best starting lineup's season FPG (`engine.rs:669-693`), where each player's value is `fpg_season` (`crates/fm-core/src/values.rs:103-108`). A season-horizon waiver gain is the added player's FPG minus the dropped player's (`waivers.rs:535-537`). Team strength in the outlook is the same lineup sum times 3.4 games (`outlook.rs:36`, `outlook.rs:199`). None of these looks at the schedule or at who fills a slot on a night a starter is idle. The this-matchup path (`waivers.rs:253-291`) does it properly, so the method already exists.
- **Example:** `fm trade-eval -l espn --give "Brandon Bussi" --get "Jake DeBrusk"` returns "Your lineup: +0.00 pts/game, +0.0 pts/week, +0 rest of season". Bussi is my third goalie: 1.78 points per start, 42% start share. Hellebuyck and Skinner start 67% and 70% of their teams' games, so two goalie slots are often open and Bussi fills them. The engine prices that at nothing and gives him a VORP of -0.34, which makes him the first drop candidate.
- **Why it matters:** every trade verdict, every season-long add or drop and the playoff odds use this number. Goalies are hit hardest, and more so on Fantrax where a start averages about 5.9 points ([03 section 7](03-hockey-player-value-drivers.md#7-how-our-scoring-shifts-value)).
- **Suggested change:** compute, per roster, each player's expected started games over the horizon by solving the daily lineup over the remaining schedule (the existing daily plan, run over more days and cached), and value a roster as the sum of rate times started games. A cheaper first step is a start-fraction per player from the next four weeks. **Size:** large; the first step is medium.

### B2. An injured player is valued as if his slot were empty while he is out

- **Category:** (b). **Confidence:** confirmed in code and by CLI.
- **Research:** with a free IR slot, an injured player is worth his games plus replacement production for the games missed ([02 implication 5](02-roster-construction-and-drafting.md#implications-for-the-engine), [05 section 5](05-in-season-management.md#5-injuries)).
- **Code:** season value is `fpg` times an availability multiplier (`valuate.rs:945-946`, `valuate.rs:1140`), and VORP subtracts the full replacement level from that (`valuate.rs:1327`). There is no check for a free IR slot in valuation. The hold-or-cut module does the comparison correctly (`injuries.rs:321-420`), so two parts of the program can disagree about the same player. The waiver engine lowers its bar when the dropped player is injured (`base.rs:573`, `waivers.rs:567-572`).
- **Example:** Brad Marchand sits in an ESPN IR slot: 1.77 FPG, 5 of 66 remaining games missed, season value 1.63, VORP +0.03 against a forward replacement of 1.60. With a replacement filling his five games the slot is worth about 0.92 x 1.77 + 0.08 x 1.60 = 1.76, a VORP of +0.16 (my arithmetic). Brayden Point (1.83 FPG, back December 2) shows a VORP of -0.02, below replacement.
- **Why it matters:** injured stars look droppable and cheap in trade values, on both my roster and other teams'.
- **Suggested change:** for season value use availability x FPG + (1 - availability) x replacement when the player is, or can be moved, in an IR slot; keep the current number when no IR slot is free. **Size:** small to medium.

### B3. Thresholds are fixed fantasy-point amounts, the same in both leagues, and no value carries an error bar

- **Category:** (b) and (c). **Confidence:** confirmed in code.
- **Research:** carry uncertainty on every value, wider for goalies, and call a trade too close when the range of the gain includes zero ([06 implications 7 and 8](06-trade-strategy.md#implications-for-the-engine)).
- **Code:** waiver bars 0.3 / 0.6 / 1.0 FPG (`base.rs:560-565`); lineup swap bar 0.5 points or 8% (`lineup.rs:49-50`); trade proposal bar 0.3 and offer verdicts +0.1 and -0.3 (`crates/fm-engine/src/recommend/trades.rs:74`, `trades.rs:140-141`); planned-move bars 0.5 / 1.0 / 2.0 matchup points (`crates/fm-engine/src/recommend/plan.rs:54-61`). A search for any scoring-scale factor (`fpg_scale`, `scoring_scale`, `league_scale`) in `fm-engine/src` found none. The only confidence figure is games played (`base.rs:36-50`), used to scale a score, not to widen a verdict.
- **Example:** the same 0.3 FPG is 19% of an ESPN replacement forward (1.60) and 7% of a Fantrax one (4.45). A Fantrax offer is "accept" at +0.1 on a lineup total of 51.7, which is 0.2%.
- **Why it matters:** Fantrax recommendations fire on edges that are well inside projection noise. The backtest's own mean absolute errors are 0.23 FPG for forwards and 0.58 for goalies under ESPN scoring (`crates/fm-engine/data/fitted_params.json`, `mae_at_fitted_k_multi`).
- **Suggested change:** express every threshold as a multiple of a per-league unit (for example the replacement forward's FPG, or the backtest error for the position), and attach that error to each player value so a trade gain can be reported as a range. **Size:** medium.

### B4. A role change moves the projection by a small fixed step, and measured ice time does not move it at all

- **Category:** (b). **Confidence:** confirmed in code.
- **Research:** role is the most stable and most observable input; a move from the second to the first power-play unit roughly triples power-play points, and a role change should move the projection immediately in proportion to the minutes gained ([03 section 1](03-hockey-player-value-drivers.md#how-fast-a-role-change-shows-up), [04 section 6](04-projection-and-regression.md#6-in-season-updating)).
- **Code:** a top role multiplies offensive rates by 1.15, a minor one by 1.05, a depth line by 0.92, and half of the change carries into the season (`crates/fm-engine/src/valuation/roles.rs:16-24`, `roles.rs:143-277`). The signal is the Daily Faceoff line or power-play unit against the player's usual one, or news. Measured ice-time and power-play-share trends are used only for alerts (`crates/fm-engine/src/recommend/flags.rs:329-480`); a search for `toi_trend` and `pp_share_trend` in `valuation/` finds nothing.
- **Example:** CLI shows Lawson Crouse moved from the third to the second line: x1.05 this week, x1.02 for the season.
- **Why it matters:** pickups after a real promotion are found late, which is where waiver value comes from in a league with two or three moves a week.
- **Suggested change:** project power-play points from power-play share times team output, and scale shots, hits and blocks by projected minutes, using the deployment data already stored. Validate in the backtest. **Size:** large.

### B5. Lineup and move choices maximise expected points; goalie starts in the lineup ignore the opponent; the win probability is only reported

- **Category:** (b) and (c). **Confidence:** confirmed in code.
- **Research:** maximise win probability; a favourite should sit a goalie whose downside could flip a nearly decided week, and an underdog should start marginal ones ([08 section 4](08-h2h-objectives-and-game-theory.md#4-opponent-aware-decisions)).
- **Code:** the daily lineup maximises the sum of expected day values (`lineup.rs:690-740`). A goalie's day value is his per-start FPG times the start probability, floored at zero, with no opponent or win-probability term (`lineup.rs:557-586`). A separate goalie planner does adjust for the opponent's goals and the betting win probability (`crates/fm-engine/src/analysis/goalie_plan.rs:127-157`), but only `fm goalies --week`, the weekly plan and the dashboard call it (`crates/fm-cli/src/cmd_recs.rs:1160`, `plan.rs:1392`, `crates/fm-web/src/views.rs:1564`). The matchup stance (chase below 40%, protect above 60%) changes advice text (`crates/fm-engine/src/analysis/matchup.rs:713-800`) and the planned-move bar (`plan.rs:54-61`), not the lineup. The weekly plan's search maximises expected matchup points (`plan.rs:791-832`).
- **Why it matters:** on ESPN an average start is worth about 1.7 points with real downside, so a below-average goalie against a strong offence can have a negative expectation that the lineup solver never sees. The variance effect itself is small ([08 section 2](08-h2h-objectives-and-game-theory.md#2-the-weekly-objective-win-probability-not-expected-points)).
- **Suggested change:** first use the goalie planner's points-if-he-starts in the lineup's day value, without the zero floor. Then, for the last day or two of a matchup, compare win probability with and without each goalie start. **Size:** small for the first step, medium for the second.

### B6. Dynasty value is built on raw FPG with one replacement level for every position

- **Category:** (b). **Confidence:** confirmed in code; size depends on the precondition above.
- **Research:** value is over replacement at the player's position; goalies get no growth before about 25 and decline from about 30; goalie pedigree is weak; a prospect needs an arrival probability and a roster-slot charge ([07 implications 3, 4, 8 and 9](07-dynasty-and-keeper.md#implications-for-the-engine)).
- **Code:** the trajectory multiplies `fpg_season`, not VORP (`dynasty.rs:338`, `dynasty.rs:381`). The keeper surplus and the trade engine's roster-spot value both use the lowest replacement level across all slots (`dynasty.rs:298-309`, `engine.rs:74-80`). The goalie curve is hand-set and rises from 0.55 at 18 to 1.00 at 27 (`dynasty.rs:38-39`). Pedigree is the same for goalies and skaters and fades at 200 games for everyone (`dynasty.rs:41-43`, `dynasty.rs:152-193`), while file 07 gives 400 games for defensemen. Unproven players do get an expected games share (`crates/fm-engine/src/valuation/rookie.rs:84-90`), but there is no bust probability on later seasons.
- **Example:** Fantrax replacement is 4.45 for forwards and 3.64 for defensemen. A 5.0 FPG forward (VORP 0.55) carries more future value than a 4.5 FPG defenseman (VORP 0.86).
- **Suggested change:** build the future part from value over the player's own positional replacement, flatten the goalie curve before 25, and weaken goalie pedigree. **Size:** medium.

### B7. The acceptance model treats an even offer as a coin flip and has never been calibrated

- **Category:** (b), with one unfinished piece. **Confidence:** confirmed in code.
- **Research:** owners ask more for their own players than they would pay; an offer that is even by market value is declined more often than accepted ([06 implication 11](06-trade-strategy.md#implications-for-the-engine)).
- **Code:** acceptance is a logistic curve with midpoint 0, so an even offer is 50% (`trades.rs:102-105`, `crates/fm-engine/src/recommend/trades/accept.rs:49-59`). The package rule (best player in full, others at half, `accept.rs:32-36`) and the convex worth (`accept.rs:15-18`) do model the star premium. A calibration function exists in `accept.rs`, but a search of all crates for `calibrate_acceptance` finds only its definition, its re-export and tests. Proposals are filtered on my edge and on acceptance, not on the other side's gain by our model (`engine.rs:336-349`), which is the opposite of [06 implication 14](06-trade-strategy.md#implications-for-the-engine).
- **Suggested change:** move the midpoint so an even offer sits at roughly 35 to 40% until data exists, and wire the calibration into the harness refit step. **Size:** small.

### A1. The harness matches and grades Fantrax lineup advice as if lineups locked weekly on Monday

- **Category:** (a). **Confidence:** confirmed in code and by CLI.
- **Code:** `first_lockable_day` moves any Fantrax date to the next Monday (`crates/fm-harness/src/episode_match.rs:212-219`). Lineup matching looks at snapshots one to six days after that Monday (`episode_match.rs:430-434`), and grading starts its window there and counts every day of it (`outcomes.rs:600-625`). The engine reads this league's lock as daily (`crates/fm-engine/src/recommend/lineup_lock.rs:17-24`; CLI "Lineup changes: daily"), so its Fantrax lineup advice is about today.
- **Why it matters:** a "start X today" recommendation made on a Tuesday is graded on the following Monday's games. Fantrax lineup hit rates and followed-or-ignored labels are therefore unreliable. Recommendations themselves are not affected.
- **Suggested change:** pass the league's lock mode into the matcher and grader and use the ESPN branch when it is daily. **Size:** small.

### B8. One shrinkage constant for every statistic, fitted under ESPN scoring and used for both leagues

- **Category:** (b). **Confidence:** confirmed in code; effect unverified.
- **Research:** volumes stabilise an order of magnitude faster than percentages ([04 section 1](04-projection-and-regression.md#1-regression-to-the-mean-and-stabilisation)).
- **Code:** `shrink_toward` applies the same n / (n + k) to every stat key (`blend.rs:86-108`), with k = 25 games for skaters and 40 for goalies in season (`params.rs:31`). Goals, assists and saves get their own extra corrections (see D1), so the remaining issue is shots, hits, blocks and penalty minutes, which are over-regressed. The constants come from a fit that minimised FPG error under ESPN weights (`fitted_params.json`, `"scoring": "espn"`); `fm settings -l fantrax` prints "Valuation params: fit 2026-09-28 (espn)". Fantrax pays five times as much per shot.
- **Suggested change:** fit k per statistic and per scoring preset in the rolling-origin backtest, and keep it only if it wins out of sample. **Size:** medium.

### C1. Goalie wins and shots faced come from the goalie's own history, and nothing resets when a player changes teams

- **Category:** (c). **Confidence:** likely.
- **Research:** project wins and shots faced from the team ([03 implications 6, 7 and 14](03-hockey-player-value-drivers.md#implications-for-the-engine)).
- **Code:** goalie rates, wins included, are the blend of the provider projection, the goalie's history and season to date (`valuate.rs:179-231`, `valuate.rs:296-311`). Only the separate goalie planner replaces wins with a market win probability (`goalie_plan.rs:147-155`). A search for team-change handling in `valuation/` and `crates/fm-providers/src/enrich.rs` (`team_change`, `prev_team`, "traded to") found nothing.
- **Example:** CLI lists Hellebuyck with NJD, while his baseline is "60% projection + 40% NHL history (180 GP over 3 seasons)". Whether the provider projection already reflects the new team is unverified.
- **Suggested change:** project wins per start and shots against from team rates, keep only the regressed save talent from the goalie. **Size:** medium.

### B9. The matchup simulation treats players as independent and its spread does not scale with the scoring

- **Category:** (b). **Confidence:** confirmed in code; which league is off is unverified.
- **Code:** each starter is an independent normal draw (`matchup.rs:327-371`) with a per-game standard deviation of 1.3 x sqrt(FPG) for skaters and 2.5 x sqrt(FPG) for goalies (`matchup.rs:39-41`, `matchup.rs:187-189`). If every scoring weight doubles, real spread doubles but this formula grows by 1.41, so the two leagues cannot both be calibrated. A search for "correlat" and "covarian" in `fm-engine/src` finds nothing. The harness fits a per-league scale but never applies it (`matchup_calib.rs:32-39`, `docs/harness.md`).
- **Suggested change:** apply the fitted scale per league once eight matchups are graded, and add a shared team factor for linemates later. **Size:** small, then medium.

### C2. No season-level leverage, no luck ledger, and tiebreakers are not used

- **Category:** (c). **Confidence:** confirmed by search.
- **Code:** searches for "leverage", "all-play", "expected wins" and "schedule luck" across all crates find nothing. `points_for` is read for Fantrax standings (`fantrax.rs:1541-1567`) and not used by the engine. The playoff simulation breaks ties on projected strength (`outlook.rs:242`); ESPN's tie and seeding rules are checked for presence and discarded (`crates/fm-providers/src/espn/api.rs:937`, `api.rs:940`). The simulation gives playoff odds but no title odds.
- **Why it matters:** my ESPN team is 1-5 with about 12% playoff odds and eight playoff spots. The research says the first job is to tell bad luck from a weak roster ([08 section 7](08-h2h-objectives-and-game-theory.md#7-a-team-that-is-far-behind)). The engine's lineup rank (12 of 14) is a forward-looking answer, but points-for and all-play are not shown beside it.
- **Suggested change:** add points-for rank and all-play record to `fm outlook`, use points for as the tiebreak, and report the change in playoff odds from winning this week. **Size:** small to medium.

### A2. Constants assume an 82-game season; 2026-27 has 84

- **Category:** (a). **Confidence:** confirmed in code and by CLI (the loaded schedule has 1,344 games, which is 32 x 84 / 2).
- **Code:** the projected goalie share divides projected starts by 82 (`valuate.rs:56`, `valuate.rs:587`), so a 52-start projection reads 63% where 52 / 84 is 62%. The no-schedule fallbacks use 82 games in 186 days (`crates/fm-engine/src/valuation/adjust.rs:22-23`, `base.rs:739-740`, `outcomes.rs:36`). A search for the number 84 in `fm-engine`, `fm-providers`, `fm-harness` and `fm-core` finds nothing.
- **Suggested change:** take games per team from the loaded schedule. **Size:** small.

### Smaller items

- **(c) Assist split.** MoneyPuck primary and secondary assists are parsed (`crates/fm-sources/src/moneypuck.rs:224-225`, `moneypuck.rs:345-346`) and used nowhere else (search of all crates). Files 03 and 04 disagree on how much this is worth, so test before building.
- **(b) Suspected: the positional mean depends on which free agents were loaded.** The shrink target is the mean over the league's loaded players with 20 or more prior games (`valuate.rs:94-116`). ESPN loads 326 free agents and Fantrax 738, so the same player is shrunk toward different means. Likely; size unverified.
- **(c) Off-night scheduling in season value.** The week value adds 5% per off-night game, counted league-wide as nights with fewer than 8 games (`schedule.rs:39`, `schedule.rs:104-106`, `params.rs:68`). The matchup plan is roster-aware. Season value has no schedule term, which B1 would cover.
- **(b) The last-move reserve does not expire.** The 1.0 FPG bar for the final move applies on the last day of the period too (`base.rs:616-624`), when an unused move is worth nothing.

### Research claims about the engine that the code shows to be wrong

- **D1. "One k per position on total fantasy points per game" ([04](04-projection-and-regression.md#how-this-applies-to-our-model)).** Shrinkage is applied to each statistic's rate (`blend.rs:86-108`), and goals, assists and saves have separate regressions (`regression.rs:16-32`). What remains true is B8.
- **D2. "GSAx may reward one season as skill" ([04](04-projection-and-regression.md#how-this-applies-to-our-model)).** Goals saved above expected (GSAx: expected goals against minus actual goals against) is pooled over three seasons and regressed with 3,000 shots (`regression.rs:288-326`), then the save component is rebuilt from league save percentage, shot quality and that regressed figure (`regression.rs:333-382`). CLI: Hellebuyck, +39.6 on 3,326 shots, is regressed to +0.46 per 100 shots and his projected save percentage falls from .910 to .901. This is the treatment the research asks for. Goalies with fewer than 500 weighted shots skip it (`regression.rs:31`) and fall back to k = 40 games.
- **D3. "Missing: NHLe prior" ([04](04-projection-and-regression.md#how-this-applies-to-our-model)).** It exists (`rookie.rs:31-38`, `rookie.rs:174`, `rookie.rs:281`, `crates/fm-sources/data/nhle_factors.json`).
- **D4. "Missing: an explicit role-change path" ([04](04-projection-and-regression.md#how-this-applies-to-our-model)).** It exists (`roles.rs:143-277`); its size is the issue (B4).
- **D5. "The goalie age factor is hand-set" ([04](04-projection-and-regression.md#how-this-applies-to-our-model)).** The season projection has no goalie age factor at all (`params.rs:33`); only the dynasty goalie curve is hand-set (`dynasty.rs:38-39`).
- **D6. "Replacement should be F126 to F140 on ESPN, F72 on Fantrax" ([01 implication 2](01-fantasy-fundamentals.md#implications-for-the-engine)).** The engine uses the real free-agent pool, which is that file's own implication 13. The two implications conflict; the engine follows the better one.
- **D7. "The trade evaluator needs a regression test for the goalie-plus-winger-for-centre case" ([06 implication 17](06-trade-strategy.md#implications-for-the-engine)).** The goalie slot is solved exactly, so a starting goalie's loss is counted. A search of test names in `trades/tests.rs` for "goalie" found no such fixture, so the test is still missing even though the logic is there. The uncounted case is the bench goalie (B1).

## Corrections to the knowledge files

1. **01, "How this applies" and "Open questions"; 03, "Open questions"; 05, section 1 "Lineup locks".** Fantrax lineups in this league change daily (read from the Rules page, `fantrax.rs:3600`).
2. **02 "How this applies", 06 "How this applies", 07 "How this applies".** The Fantrax bench is not six deep in practice. With 12 active players the 16-player cap leaves four reserve spots. File 05 already uses 16.
3. **02, 03, 06 "2 moves per week" for ESPN.** It is 2 adds per matchup period; the period is 7 days in the regular season.
4. **03 line 11 and 07 "Open questions".** Both link to `README.md#league-facts-confirmed-from-the-app`. There was no `README.md` in `docs/knowledge/` when this file was written.
5. **04 "How this applies to our model".** See D1 to D5: the single-k statement, the GSAx doubt, the three "missing" items for NHLe and role change, and the goalie age factor.
6. **04 implication 4.** "600 shots of .930 should project within about .003 of his prior" does not follow from the file's own n / (n + 3,000) rule, which gives about .006 for a goalie .034 above the mean.
7. **05 "Open questions", league-specific unknowns.** Most are now known; see the next section.
8. **07 throughout.** The file assumes every player is kept. The program is configured to keep three. Confirm with the owner, then rewrite either the file's league section or the configuration.
9. **07 "How this applies", first bullet.** "Before hits, blocks and faceoffs": no faceoff statistic is scored. FT is fights.
10. **08 "How this applies", ESPN.** Playoff teams and regular-season length are no longer unknown: 8 of 14 teams, 18 regular-season weeks left on 2026-11-10. The file's own table gives 14 to 23% for a 1-5 team with eight spots; the engine says about 12% for this roster.
11. **08 section 2.** The goalie example uses ESPN default scoring (win 4, goal against -2). This league uses win 2, goal against -1.5.

## Open league facts still unknown

| Fact | Does the program read it? | Value or what is needed |
|---|---|---|
| Playoff teams | Yes. ESPN `playoffTeamCount` (`api.rs:932`), Fantrax Rules page (`fantrax.rs:3602-3604`), used at `outlook.rs:172-188` | ESPN 8, Fantrax 6 |
| Playoff weeks | Yes | ESPN periods 25 to 27, March 22 to April 10 (CLI `fm playoffs`). Fantrax starts scoring period 25, March 22. Fantrax round lengths were not shown: unverified |
| Trade deadline | Yes (`api.rs:934`, `playoffs.rs:235`) | ESPN February 26, 2027. Fantrax "Not Used" |
| Lineup lock | Fantrax yes (`lineup_lock.rs:17-24`). ESPN no: assumed daily (`lineup_lock.rs:36-38`), with per-game locks from game start times (`lineup.rs:589-592`) | Whether ESPN adds lock at the day's first game is unverified |
| Waiver system | Yes. ESPN acquisition settings (`crates/fm-providers/src/espn/mod.rs:519-548`), Fantrax Rules page (`fantrax.rs:3617-3620`) | ESPN waiver priority, 24 hours after a drop, my priority 7 of 14. Fantrax free agents first come first served, waivers 2 days in priority order, order rotates |
| What counts as a move | Partly. The used and remaining counts are read | Whether drops, trades and IR moves are free was not checked. Move limits in playoff rounds: unverified |
| Seeding and matchup tiebreakers | No. ESPN fields are present but discarded (`api.rs:937`, `api.rs:940`); Fantrax not read | Owner must supply, or the code must read them |
| Trade review or veto | No (`api.rs:930` checks presence only). Fantrax prints "Trade Voting System: Managers" | Thresholds unknown |
| IR eligibility | No. ESPN assumed IR, LTIR or out (`lineup.rs:55`); roster moves to IR assume IR or LTIR (`base.rs:71`) | Owner must confirm the Fantrax rule |
| Keepers per team (Fantrax) | No. Owner setting `FANTRAX_KEEPERS` (`snapshot.rs:43`) | Currently 3; the Rules page says Dynasty. Owner must confirm |
| Season length | The schedule is read (84 games); several constants say 82 | See A2 |

## Suggested order of work

1. Confirm the Fantrax keeper rule with the owner. It decides how much of B6 and file 07 matters.
2. B2: value injured players with replacement fill-in when an IR slot is available. Small, and it removes wrong drop and trade signals now.
3. B5, first step: use the goalie planner's matchup-aware start value in the daily lineup, without the zero floor.
4. A1: make the harness use the league's real lineup lock for Fantrax, so Fantrax lineup grades mean something.
5. B3: scale every threshold to the league's scoring and attach a per-position error to values; report trade gains as a range.
6. B1: value rosters by started games over the schedule, starting with a four-week start fraction, and use it in trades, season waivers and the outlook.
7. B7 and B9: shift the acceptance midpoint, wire in the acceptance calibration, and apply the fitted matchup spread per league.
8. B4: project power-play points from share and scale volume statistics by minutes; validate in the backtest.
9. B8 and C1: fit k per statistic and per scoring preset; project goalie wins and shots against from the team.
10. C2 and A2: add points-for rank, all-play record and weekly leverage to the outlook, use the real tiebreak, and read games per team from the schedule.

## Limits of this review

Read in full or in the cited parts: the eight knowledge files, `docs/ARCHITECTURE.md`, `docs/backtesting.md`, `docs/harness.md`, `docs/trades.md`, and the code lines cited above. Only outlined, not read: `analysis/playoffs.rs`, `analysis/schedule_grid.rs`, `recommend/flags.rs`, `recommend/claims.rs`, `recommend/alerts.rs`, `recommend/market.rs`, `recommend/plan_memory.rs`, the trade search, need, target and exploit modules, `valuation/picks.rs`, `valuation/keepers.rs`, most of `valuation/rookie.rs`, `fm-harness/src/deployment.rs`, `fm-harness/src/refit.rs` and all of `fm-backtest`. No test was run. How ESPN's HAT and DEF statistics reach the projected rates was not traced. The size of every gap is a judgement from the code and a handful of CLI examples, not a measurement; the backtest and the harness are the places to measure them.
