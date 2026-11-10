# 01 - Fantasy sports fundamentals (cross-sport)

Researched 2026-11-10. Every factual claim links to the page it came from. Where a claim was seen only in a search-result excerpt and not on the fetched page, it is marked "(excerpt only)". Arithmetic and reasoning that are mine rather than a source's are marked "(inference)".

## What this covers

This document collects the ideas that are common to fantasy football, baseball, basketball and hockey: what each league format rewards, how player value is defined (replacement level, value over replacement, Value Based Drafting, auction dollars, tiers), what winning a head-to-head league really requires, how to think about roster spots and depth, how the market (average draft position, trade partners) misprices players, and which popular beliefs do not survive checking. It ends with what this means for the owner's two head-to-head points leagues and with concrete rules the engine should follow.

## Key takeaways

- **A player's value is his margin over the alternative, not his raw points.** This is the founding principle of Value Based Drafting, stated by Joe Bryant in 2000 ([FFToday](https://fftoday.com/guest/joe/principles_vbd.htm)), and every later method (VORP, VOLS, VONA, auction dollars) is a choice of which "alternative" to subtract.
- **The baseline depends on league size and starting slots far more than on scoring weights.** Bryant says team count and lineup requirements change values more than the scoring system does ([FFToday](https://fftoday.com/guest/joe/principles_vbd.htm)).
- **In a daily-lineup league with no games cap, value is per game times usable games, not season total.** Standard calculators assume a lineup that never changes ([Pitcher List](https://pitcherlist.com/introducing-total-daily-vorp-a-value-system-for-daily-lineups/)); ESPN's own analyst says that with limited moves "quantity will usually beat quality" for all but top players ([ESPN](https://africa.espn.com/fhl/story/_/id/35196481/espn-nhl-how-add-drop-stream-max-your-team)).
- **In head-to-head points the objective is still, overwhelmingly, to score more expected points.** Variance and consistency are second-order: "Variance matters. But not much" ([Footballguys, Harstad](https://www.footballguys.com/article/2026-dynasty-in-theory-week06-13-theses-on-variance)).
- **Week-to-week consistency is neither valuable nor a stable player trait** in the data examined (year-to-year correlation of 0.08 in one sample) ([Footballguys](https://www.footballguys.com/article/2026-dynasty-in-theory-week06-13-theses-on-variance)).
- **Variance preference depends on whether you are the favourite or the underdog**: "for the underdog, variance is a friend" and "for the favorite, variance is the enemy" ([Footballguys, Tremblay](https://www.footballguys.com/article/DFS_expectationvariance)).
- **Playoffs are close to coin flips.** The higher seed wins about 58% of dynasty playoff matchups and "the best team loses in about two-thirds of leagues" ([Footballguys](https://www.footballguys.com/article/2023-dynasty-in-theory-83-skill-vs-luck)). The controllable goal is making the playoffs with the strongest possible roster.
- **Win-loss record is a noisy measure of team quality; points scored is the better one**, and schedule luck does not persist ([Footballguys](https://www.footballguys.com/article/HarstadFiT7), [Footballguys](https://www.footballguys.com/article/2021-regression-alert-week-10)).
- **A roster spot has value equal to the best free agent you could put in it.** A 2-for-1 trade is really "two players for one player plus the free agent who fills the open spot" ([ESPN, Cockcroft](https://africa.espn.com/fantasy/baseball/story/_/page/mlbdk2k16_multitrade/fear-multi-player-trade-offers-mlb)).
- **Positional scarcity is a property of the replacement level at each position**, not of how many stars a position has ([The Hardball Times](https://tht.fangraphs.com/what-is-position-scarcity/)). If replacement level is computed correctly, no extra "scarcity bonus" is needed.
- **Keeper value is surplus (value minus cost), and keeper leagues inflate the price of everyone else** ([FanGraphs](https://fantasy.fangraphs.com/on-surplus-and-inflation-in-keeper-leagues/)).
- **Goalies are the one position that can subtract points**, which changes streaming logic ([ESPN](https://africa.espn.com/fhl/story/_/id/35196481/espn-nhl-how-add-drop-stream-max-your-team)).
- **Fantasy is mostly skill over many trials, but hockey sits nearer the luck end than baseball or basketball** ([MIT News](https://news.mit.edu/2018/hosoi-study-skill-fantasy-sports-1107)). Process should be judged over a season, not a week.

## 1. League formats and what each rewards

ESPN's help centre defines the scoring formats as follows ([ESPN Scoring Formats](https://support.espn.com/hc/en-us/articles/360003946651-Scoring-Formats)):

- **Rotisserie ("roto")**: "teams are ranked from first to last in each statistical category", and rank points are summed. The objective is season-long category ranks; there is no weekly opponent, so balance across categories matters and a surplus in one category is wasted.
- **Head-to-head (H2H) points**: each stat has a point value and each matchup is won by the team with more fantasy points. The objective is to win weekly matchups, and all stats are fungible because they convert to one currency.
- **H2H each category / most categories**: a win is credited per category, or one win goes to whoever wins the most categories. Stats are not fungible, so team "build" and punting a category become meaningful.
- **Season points**: standings come from accumulated points for the whole season. The objective is simply total season points.
- **Best ball**: there is no in-season management; the platform automatically counts each roster's best scores per position each week, so depth and "spike weeks" (unusually high single weeks) matter more than in managed leagues ([4for4](https://www.4for4.com/2026/preseason/intro-best-ball-leagues-prizes-rules-basic-strategy), excerpt only).

The practical difference between "season points" and "H2H points" is that H2H converts a point total into a sequence of win/loss results against one opponent per period. That adds schedule luck (see section 3) and makes the *timing* of points matter: points in the playoff weeks are worth more than points in week 3.

**Redraft, keeper, dynasty.** In redraft every player returns to the pool each year, so only this season counts. Keeper and dynasty leagues carry some or all players forward. Harstad splits dynasty value into "actual value" (points in your lineup now) and "perceived value" (what the trade market will pay), and argues a roster needs both because "Perceived value is what allows a roster to sustain actual value over an extended timeframe" ([Footballguys](https://www.footballguys.com/article/HarstadDynastyValue)). In keeper formats the unit of value is **surplus**: "a surplus asset is one where their expected production exceeds their cost" ([FanGraphs](https://fantasy.fangraphs.com/on-surplus-and-inflation-in-keeper-leagues/)).

**Daily vs weekly lineups.** With weekly locks you choose a lineup once and take whatever games those players have. With daily lineups the bench becomes productive, because a bench player can be started on a night when a starter is idle. Standard valuation tools ignore this: "Your favorite auction calculator assumes you're never changing your lineups" ([Pitcher List](https://pitcherlist.com/introducing-total-daily-vorp-a-value-system-for-daily-lineups/)). Season-long games-played caps reverse the logic again: under a cap you win by maximising points *per game* and using every allowed game ([FanGraphs](https://fantasy.fangraphs.com/ottoneu-2023-replacement-level/)); without a cap, extra games are free points.

## 2. Core valuation ideas

**Value Based Drafting (VBD).** The earliest dated statement I could read is Joe Bryant's "Principles of VBD", dated 8/22/00: "The value of a player is determined not by the number of points he scores, but by how much he outscores his peers at his particular position" ([FFToday](https://fftoday.com/guest/joe/principles_vbd.htm)). Footballguys pages variously date the idea to 1996 or "the mid-90s" (excerpt only), so the exact origin year is not settled. The method: project every draftable player's season points, pick a baseline player per position, and subtract the baseline's points. Bryant's default is that "The simplest way to set a baseline is to use the worst starter at each position", for example the 12th QB and 24th RB in a 12-team, 1-QB, 2-RB league, and he lists alternatives (last player drafted, average starter). He also warns that the resulting list is not a draft order: "don't draft a player any earlier than your fellow owners force you to" ([FFToday](https://fftoday.com/guest/joe/principles_vbd.htm)).

**Variants are just different baselines** (definitions from FantasyPros, excerpt only; its help page returned HTTP 403):

- **VOLS (value over last starter)**: baseline is the worst starter, which is Bryant's default.
- **VORP (value over replacement player)**: baseline is the best player freely available on waivers.
- **VONA (value over next available)**: baseline is the best player at that position expected to be left at your next pick, so it only exists during a draft.

Academic work formalises the VONA idea: Fry, Lundberg and Ohlmann model a draft as a stochastic dynamic program in which a pick depends on the player's value, the other players available and team need, and report that their heuristic beats best-player-available and needs-based drafting in simulated 2005 drafts given good information about opponents ([abstract, RePEc](https://ideas.repec.org/a/bpj/jqsprt/v3y2007i2n5.html); I read only the abstract).

**Expected VBD.** Chase Stuart pointed out that projections are point estimates and that real outcomes below the baseline are simply benched, so value should be the average of outcomes with negatives floored at zero. Using ADP data from 2000, the top-drafted RB returned about 127 points of expected VBD and the 12th about 46, while kickers and defences were nearly flat (about 8 to 6), which is why flat positions should be drafted last ([Footballguys](https://footballguys.com/article/stuart_expected_vbd_by_adp)). The general lesson is that value is convex in uncertainty: a volatile bench player has more expected value than his mean projection suggests, because the bad outcomes cost nothing.

**Replacement level in points leagues.** Todd Zola calls ranking by raw points "a huge mistake" and sets the baseline at the last starter in each pool (in a 10-team league with 13 hitters and 9 pitchers: the 10th catcher, 120th other hitter, 90th pitcher). After this adjustment, pitchers rise to the top in nearly every points system he examined, which shows how much the baseline moves rankings ([RotoWire](https://www.rotowire.com/baseball/article/the-z-files-points-league-pointers-38038)). FanGraphs' Lucas Kelly sets replacement level on a *per-game* basis and counts bench players too: rosterable players = teams x (starters + bench) at each position, and replacement is the next player down. Commenters objected that this ignores utility slots and multi-position eligibility ([FanGraphs](https://fantasy.fangraphs.com/ottoneu-2023-replacement-level/)). So the sources agree on the principle and disagree on where exactly to draw the line.

**Bench and streaming effects.** Zola notes that because roster moves are frequent, value in some positions "attaches to a role rather than a specific player", meaning the true replacement is a rotating stream of free agents with good matchups, and admits he does not know the size of the effect ([RotoWire](https://www.rotowire.com/baseball/article/the-z-files-points-league-pointers-38038)). Pitcher List's "Total Daily VORP" handles daily lineups by valuing per-game contribution times games played, adding an expected bench contribution, and then shifting so the last drafted starter is zero; under it, a player who misses time loses less value than in season-total systems because a replacement fills his empty days ([Pitcher List](https://pitcherlist.com/introducing-total-daily-vorp-a-value-system-for-daily-lineups/)).

**Positional scarcity.** Jonathan Halket defines it precisely: a position is scarce when its replacement-level player is worse than another position's, and "Scarcity has nothing to do with the distribution of talent above replacement level within the position" ([The Hardball Times](https://tht.fangraphs.com/what-is-position-scarcity/)). ESPN's analysts call scarcity "one of the more overrated factors" in drafts ([ESPN](https://africa.espn.com/fantasy/baseball/story/_/id/22585621/fantasy-baseball-how-handle-positional-scarcity), excerpt only). In other words, scarcity is already inside a correctly computed value over replacement and must not be added twice.

**Auction dollar values.** The standard conversion gives every rostered player $1 and distributes the remaining budget in proportion to value above replacement. FanGraphs' version: [(team budget - $1 x roster size) / roster size] x (player's value / average value of above-replacement players) + $1 ([FanGraphs](https://fantasy.fangraphs.com/value-above-replacement-part-three/)). Dollars are useful even without an auction because they put all positions on one additive scale for trades. In keeper leagues, cheap keepers remove more talent than money from the pool, so the remaining players cost more: inflation = remaining money / remaining value, about 14.9% in the FanGraphs example ([FanGraphs](https://fantasy.fangraphs.com/on-surplus-and-inflation-in-keeper-leagues/)).

**Tiers.** A tier is a group of players whose projections are too close to separate. Boris Chen popularised clustering expert consensus ranks with a Gaussian mixture model because ordinary rankings "do not illustrate the true distance between players" ([FantasyPros](https://www.fantasypros.com/2014/12/visualized-rank-tiers-week-14/), excerpt only). Tiers are a display of uncertainty, not a separate value theory.

## 3. What winning a head-to-head league requires

Winning has two stages with different mathematics: a long regular season, where the better team's edge accumulates, and a short single-elimination playoff, where it mostly does not.

**The regular season rewards points scored.** Harstad compares head-to-head record, all-play record (your record if you played every team every week) and total points, and concludes "If you want to know how good your team is, just check how much it's scored" ([Footballguys](https://www.footballguys.com/article/HarstadFiT7)). Schedule luck, the gap between actual wins and all-play expected wins, is "entirely driven by random chance" and does not carry forward ([Footballguys](https://www.footballguys.com/article/2021-regression-alert-week-10)). In one six-season ESPN league the 2.5th and 97.5th percentiles of actual minus all-play win percentage were about plus or minus 22% ([Tony ElHabr](https://tonyelhabr.rbind.io/posts/fantasy-football-performance/), excerpt only).

**The playoffs are close to coin flips.** Harstad reports the higher seed wins about 58% of dynasty football playoff matchups and "The best team loses in about two-thirds of leagues" ([Footballguys](https://www.footballguys.com/article/2023-dynasty-in-theory-83-skill-vs-luck)). A simulation of a 4-team playoff gives a team with a 53% per-game edge a 28% title chance against a 25% random baseline; a first-round bye helps far more than longer rounds ([The Fantasy Footballers](https://www.thefantasyfootballers.com/articles/the-fantasy-architect-how-to-reduce-playoff-randomness-in-your-league/)). These numbers are from football, where a matchup is one game per player; a hockey week contains three or four games per skater, which should reduce weekly noise somewhat, but I found no hockey-specific measurement (unverified).

**Expected points vs variance.** A projection is a probability-weighted mean; two players with the same mean can have different spreads, and team variance depends on correlation between players, not just the sum of individual variances ([Footballguys](https://www.footballguys.com/article/DFS_expectationvariance)). The rule for risk is: "for the underdog, variance is a friend... for the favorite, variance is the enemy" ([Footballguys](https://www.footballguys.com/article/DFS_expectationvariance)). Harstad's thirteen theses put bounds on how much this matters ([Footballguys](https://www.footballguys.com/article/2026-dynasty-in-theory-week06-13-theses-on-variance)):

- Consistency is not valuable: even perfect consistency "barely moves the needle in wins and losses".
- Consistency is not consistent: year-to-year correlation of the coefficient of variation was 0.08 among top running backs in 2023 and 2024.
- Player variance largely cancels at team level; team variance is what matters, and it can be raised or lowered through correlated players.
- Weekly variance ("consistency") and season-long variance ("upside") are different things. Season-long variance is good, because finishing 1st and 12th in alternate years beats finishing 6th and 7th.
- "Variance is still subordinate to production."

So floor-versus-ceiling is a tie-breaker. The exception is roster spots that do not need to start: for the end of the bench, season-long upside is what you are buying, since a failed bet is dropped at no cost (this follows from Stuart's floor-at-zero logic above).

## 4. Opportunity cost

**Roster spots.** Every roster spot's cost is the best free agent who could occupy it. Cockcroft defines replacement level as "the typical player available on the free-agent list" and shows that a 2-for-1 consolidation trade must be scored as the star plus the free agent who fills the vacated spot, while the side receiving two players must drop someone. He also notes that replacement level differs by team and by time of year ([ESPN](https://africa.espn.com/fantasy/baseball/story/_/page/mlbdk2k16_multitrade/fear-multi-player-trade-offers-mlb)). Deeper leagues have weaker free agents, so the freed spot is worth less there.

**Depth versus stars.** I found opinion but no rigorous outcome study. FanGraphs' Ottoneu writers favour stars-and-scrubs where trading is active; RotoWire's panel says it depends on whether the room overpays for stars ([FanGraphs](https://fantasy.fangraphs.com/stars-and-scrubs-vs-balancing-a-roster-in-ottoneu), [RotoWire](https://www.rotowire.com/baseball/article/fantasy-baseball-auction-balancing-stars-scrubs-108918); both excerpt only). The structural argument for consolidation is sound: lineup slots are limited, so points above replacement concentrated in one slot beat the same total spread over a starter and a bench player. The counter-argument is injury exposure and, in daily leagues, the fact that bench players do play.

**Streaming** means using a roster spot on a rotating series of free agents chosen for schedule or matchup. Its cost is a limited transaction and the risk that "the main danger is dropping a player who later becomes valuable to someone else". ESPN's Sean Allen: with unlimited moves, stream freely, but "Limits to roster moves eliminate the advantage of off-night streaming", so limited moves should be planned around teams that play on light nights; and goalies "are the one roster position that can actually reduce your fantasy totals" ([ESPN](https://africa.espn.com/fhl/story/_/id/35196481/espn-nhl-how-add-drop-stream-max-your-team)). A Razzball sample of H2H hockey matchups found managers with ten or more extra skater-games than their opponent went 65-5-6; the sample is small, from three leagues, and the scoring format is unclear ([Razzball](https://hockey.razzball.com/?p=34688)).

**Flexibility.** A utility slot and multi-position eligibility raise the number of usable games; unused transactions expire each period, so a move not made is value forfeited (inference).

## 5. Market concepts

**ADP (average draft position) is a price.** Harstad argues a drafter who simply follows ADP would have a winning record, that reaching (taking players well before their price) loses value in aggregate, and that ADP is "a lagging indicator" ([Footballguys](https://footballguys.com/article/HarstadADP)). Lee and Liu studied 1,350 Sleeper leagues: draft position correlated with points at about -0.56 for running backs and -0.55 for wide receivers, so the market is informative but far from exact; drafters herd on the immediately preceding pick for quarterbacks, kickers and defences; the three most common roster compositions covered about 60% of teams, and rarer compositions with fewer kickers and defences won more ([Judgment and Decision Making](https://www.cambridge.org/core/journals/judgment-and-decision-making/article/drafting-strategies-in-fantasy-football-a-study-of-competitive-sequential-human-decision-making/2AB841B3F446833348D784C0FC54DAD2)).

**Priors versus recent results.** A Footballguys study reportedly found preseason ADP and early-season performance about equally predictive of the rest of the season, with a blend beating either alone in four of five tests ([Footballguys](https://www.footballguys.com/article/HarstadFiT3), excerpt only, not verified on the page). This supports Bayesian updating: start from the preseason projection and move toward in-season results gradually.

**Documented manager biases.** Cognitive scientist Renee Miller lists the endowment effect ("overvaluing things we have invested in"), recency bias ("overvaluing things that have happened most recently"), the primacy effect (week one looms large) and confirmation bias ([University of Rochester](https://www.rochester.edu/newscenter/cognitive-bias-definition-examples-fantasy-sports-532612/)). These are where trade and waiver edges come from: buy players whose price fell on a short cold stretch, sell those whose price rose on a hot one, and discount your own attachment to players you drafted. I found no peer-reviewed measurement of these biases in fantasy trades specifically.

## 6. Pitfalls and myths

- **"Draft/rank by total points."** Wrong in any league with positions; value is relative to replacement ([RotoWire](https://www.rotowire.com/baseball/article/the-z-files-points-league-pointers-38038)).
- **"Consistent players win more matchups."** Not supported, and consistency does not repeat ([Footballguys](https://www.footballguys.com/article/2026-dynasty-in-theory-week06-13-theses-on-variance)).
- **"Add a scarcity bonus."** Double counting if replacement level is right ([The Hardball Times](https://tht.fangraphs.com/what-is-position-scarcity/)).
- **"My record shows my team is good/bad."** Record contains large schedule luck; use points scored ([Footballguys](https://www.footballguys.com/article/HarstadFiT7)).
- **"Ride the hot hand."** A study of NHL playoff goalies found "no evidence for the hot-hand theory" and, if anything, a slightly negative effect of good recent saves ([arXiv](https://arxiv.org/abs/2102.09689)). I did not verify an equivalent result for skaters.
- **"Injury prone."** The label is applied inconsistently and with recency bias; past injury raises risk but individual prediction is "difficult, if not impossible" ([Footballguys](https://www.footballguys.com/article/bramelinjuryprone)). This source is argument, not a statistical study.
- **"Handcuffing"** (rostering your star's backup): Lee and Liu found little use of it and no performance benefit ([Judgment and Decision Making](https://www.cambridge.org/core/journals/judgment-and-decision-making/article/drafting-strategies-in-fantasy-football-a-study-of-competitive-sequential-human-decision-making/2AB841B3F446833348D784C0FC54DAD2)).
- **Judging decisions by outcomes.** MIT's study of FanDuel data found fantasy results are mostly skill over many contests, with hockey and football nearer the luck end than baseball and basketball because fewer scoring events let one lucky goal matter more ([MIT News](https://news.mit.edu/2018/hosoi-study-skill-fantasy-sports-1107)). The study was partly funded by FanDuel and concerns daily contests.
- **Treating a projection as exact.** Harstad estimates that not much more than half the variation in outcomes can be explained ahead of time ([Footballguys](https://www.footballguys.com/article/HarstadFiT3), excerpt only).

## How this applies to our two leagues

Everything in this section is inference from the sources above applied to the stated settings.

**Both leagues are H2H points**, so all stats convert to one currency, there are no categories to balance or punt, and the objective each week is expected points, with variance only as a tie-breaker.

**ESPN (14 teams, redraft, daily lineups, 2 moves per period, no games cap).**
- Started players: 14 x 9 = 126 forwards, 14 x 5 = 70 defensemen, 14 UTIL (filled by whichever skater pool has the better next player), 14 x 2 = 28 goalies. With 32 NHL teams, 28 goalie slots means nearly every NHL starter is in a lineup and the free-agent goalie pool is backups; the 3-goalie maximum caps hoarding at 42.
- Fourteen teams with 5 bench spots means the waiver wire is thin, so a freed roster spot is worth less than in a 10-team league, and consolidation trades are less attractive than generic advice suggests.
- Daily lineups and no cap mean the 5 bench spots produce points on nights when starters are idle. Skater value should be per-game rate times *startable* games (games falling on nights with an open slot), which favours players on teams with off-night schedules.
- Only 2 moves per period: each move must be spent where it adds the most startable games times points per game above the dropped player, planned at the start of the period.
- Goalie scoring (SV 0.2, GA -1.5, W 2) makes a bad start negative: 20 saves and 5 goals against is 4 - 7.5 = -3.5. A goalie start has to be judged on expected value, and sitting a goalie in a bad matchup can be correct.

**Fantrax (12 teams, dynasty/keeper, 6 F / 4 D / 2 G, 3 moves per week).**
- Started players: 72 forwards, 48 defensemen, 24 goalies; there is no UTIL slot. Replacement level is much higher than in ESPN (12 x 6 = 72 forwards started versus 126 plus UTIL), so stars carry a larger share of value and mid-tier forwards are near replacement.
- Goalie scoring (SV 0.25, GA -1, W 5, SO 7.5) is much friendlier: the same 20-save, 5-goal start is 5 - 5 = 0, and an average start is clearly positive. Goalie *volume* is valuable here in a way it is not in ESPN.
- Shots (0.5) and goals (4) are weighted heavily relative to ESPN; the two leagues need separate valuations from the same projections.
- Keeper/dynasty: value is multi-year and keeper decisions should rank by surplus over cost, with the rest of the pool inflated accordingly.
- Whether Fantrax lineups lock daily or weekly was not given; this changes whether reserve players add games (see open questions).

## Implications for the engine

Well supported by sources:

1. **Value = projected points minus replacement points at the player's position, per league.** Never rank by raw points. Test: changing league size or starting slots must change values even when scoring is unchanged.
2. **Replacement level is computed per position from teams x starting slots**, with the UTIL slot assigned to the best remaining skater after dedicated slots are filled. Test: ESPN baseline near F126-F140 and D70; Fantrax near F72, D48, G24.
3. **No separate positional scarcity multiplier.** Test: no code path adds a position bonus on top of value over replacement.
4. **For daily-lineup leagues, value uses per-game rate times startable games**, including bench contribution, in the manner of Total Daily VORP. Test: a player with identical per-game rate but more off-night games in the window must rank higher.
5. **A drop is part of every add and every uneven trade.** Evaluate transactions as lineup-level deltas: (roster after) minus (roster before), with the best free agent filling any opened spot and the worst player dropped for any spot needed.
6. **Floor negative contributions at zero for benchable players** when computing expected value (Expected VBD), and compute goalie starts on expected value because they can be negative.
7. **Team strength is reported as points scored (and all-play record as a luck indicator), not win-loss record.**
8. **Projections update gradually from a prior**; recent hot or cold stretches get limited weight, and no "consistency" feature is used as a value input.
9. **Keeper decisions rank by surplus (value minus keeper cost)** and apply inflation to non-kept players.

My own inference (reasonable, not directly sourced):

10. **Primary objective per period is maximum expected points; variance is a secondary knob** used only when win probability is clearly below or above 50% (raise variance as underdog, lower as favourite), and its effect should be reported as small.
11. **Season objective is playoff qualification probability, then playoff-week expected points.** Schedule-aware valuation should weight playoff weeks more as qualification becomes likely.
12. **Limited moves are an inventory.** With 2 (ESPN) or 3 (Fantrax) moves, rank candidate moves by marginal expected points over the period and do not leave moves unused when a positive option exists; charge a move the option cost of the dropped player's future value.
13. **In-season replacement level is the actual free-agent pool in that league**, not a theoretical rank, and should be recomputed daily. In the 14-team league expect it to be low.
14. **Bench spots at the margin should hold upside** (players whose value could jump with a role change) rather than safe low-ceiling players, because the downside is a free drop.
15. **Separate valuation per league from shared projections**, since scoring weights, roster shape and goalie economics differ.

## Open questions / where sources disagree

- **Where exactly to set replacement level.** Bryant uses the worst starter; Kelly counts bench players too; Zola uses the worst starter but adjusts pitchers empirically; commenters note utility slots and eligibility are ignored. The engine should make the baseline configurable and validate it against the real free-agent pool.
- **Team quality metric.** Harstad prefers total points to all-play; his Regression Alert columns use all-play to forecast record. Both agree head-to-head record is worst.
- **Goalies.** DobberHockey's Neil Parker argues goalies are more consistent year to year and that "the replacement level among skaters is also much higher than in nets", so elite goalies deserve early investment; commenters replied that goalie stats are the most volatile in fantasy sports and that the answer depends on format ([DobberHockey](https://dobberhockey.com/?p=38878)). This needs to be settled empirically with our scoring.
- **Stars versus depth.** No rigorous study found; advice is conditional on league size, trading activity and prices.
- **How much hockey weeks reduce variance** relative to the football figures quoted (58% higher-seed win rate, playoff simulation). Unverified for hockey.
- **Size of the streaming effect on replacement level.** Zola says he does not know it; Razzball's games-played result is suggestive but small and of unclear format.
- **Origin date of VBD**: 1996, "mid-90s" or 2000 depending on the page.
- **Fantrax lineup lock frequency and keeper cost rules** for the owner's league were not supplied.
- **Time discounting in dynasty.** Harstad's recent work reportedly ignores a discount rate for simplicity while other charts apply one (excerpt only).

## Sources

Read (page fetched and checked):

- https://fftoday.com/guest/joe/principles_vbd.htm
- https://footballguys.com/article/stuart_expected_vbd_by_adp
- https://www.footballguys.com/article/2026-value-based-drafting-one-strategy-behind-every-strategy
- https://www.footballguys.com/article/2026-dynasty-in-theory-week06-13-theses-on-variance
- https://www.footballguys.com/article/DFS_expectationvariance
- https://www.footballguys.com/article/2023-dynasty-in-theory-83-skill-vs-luck (visible portion; remainder paywalled)
- https://www.footballguys.com/article/HarstadFiT7
- https://www.footballguys.com/article/2021-regression-alert-week-10
- https://www.footballguys.com/article/HarstadDynastyValue
- https://footballguys.com/article/HarstadADP
- https://www.footballguys.com/article/bramelinjuryprone
- https://www.thefantasyfootballers.com/articles/the-fantasy-architect-how-to-reduce-playoff-randomness-in-your-league/
- https://support.espn.com/hc/en-us/articles/360003946651-Scoring-Formats
- https://africa.espn.com/fhl/story/_/id/35196481/espn-nhl-how-add-drop-stream-max-your-team
- https://africa.espn.com/fantasy/baseball/story/_/page/mlbdk2k16_multitrade/fear-multi-player-trade-offers-mlb
- https://www.rotowire.com/baseball/article/the-z-files-points-league-pointers-38038
- https://fantasy.fangraphs.com/ottoneu-2023-replacement-level/
- https://fantasy.fangraphs.com/value-above-replacement-part-three/
- https://fantasy.fangraphs.com/on-surplus-and-inflation-in-keeper-leagues/
- https://tht.fangraphs.com/what-is-position-scarcity/
- https://pitcherlist.com/introducing-total-daily-vorp-a-value-system-for-daily-lineups/
- https://hockey.razzball.com/?p=34688
- https://dobberhockey.com/?p=38878
- https://news.mit.edu/2018/hosoi-study-skill-fantasy-sports-1107
- https://arxiv.org/abs/2102.09689 (abstract)
- https://ideas.repec.org/a/bpj/jqsprt/v3y2007i2n5.html (abstract only)
- https://www.cambridge.org/core/journals/judgment-and-decision-making/article/drafting-strategies-in-fantasy-football-a-study-of-competitive-sequential-human-decision-making/2AB841B3F446833348D784C0FC54DAD2
- https://www.rochester.edu/newscenter/cognitive-bias-definition-examples-fantasy-sports-532612/

Seen only as search-result excerpts (not fetched or fetch failed):

- https://support.fantasypros.com/hc/en-us/articles/115005868747-What-is-value-based-drafting-What-do-player-draft-values-mean-VORP-VONA-VOLS-VBD (HTTP 403)
- https://www.4for4.com/2026/preseason/intro-best-ball-leagues-prizes-rules-basic-strategy
- https://www.fantasypros.com/2014/12/visualized-rank-tiers-week-14/
- https://tonyelhabr.rbind.io/posts/fantasy-football-performance/
- https://africa.espn.com/fantasy/baseball/story/_/id/22585621/fantasy-baseball-how-handle-positional-scarcity
- https://fantasy.fangraphs.com/stars-and-scrubs-vs-balancing-a-roster-in-ottoneu
- https://www.rotowire.com/baseball/article/fantasy-baseball-auction-balancing-stars-scrubs-108918
- https://www.footballguys.com/article/HarstadFiT3
