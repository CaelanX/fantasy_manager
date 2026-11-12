# 08 - Head-to-head objectives and game theory: what should we actually optimise?

Researched 2026-11-10. Evidence labels used throughout: **[sourced]** means the claim comes from a page that was read for this document; **[snippet]** means it was seen only in a search-result summary and the page itself was not opened; **[own calc]** means a calculation or simulation done for this document; **[inference]** means reasoning of ours that no source states directly.

## What this covers

This document is about the *objective function* of a head-to-head (H2H) points fantasy hockey manager: the chain from single lineup decisions, to winning one weekly matchup, to qualifying for the playoffs, to winning a single-elimination bracket. It covers how much of an H2H record is luck, when a manager should seek or avoid variance (spread of outcomes), how correlation between players changes that spread, opponent-aware tactics and their limits, how to spend scarce moves, what changes in the playoffs, what a team that is far behind should do, and how to judge decisions separately from results. Very little of the published work is about season-long fantasy hockey. Most quantitative evidence comes from fantasy football, daily fantasy sports (DFS, one-day contests with a salary cap), or real sports, and each claim below says which.

## Key takeaways

- The thing to maximise each week is the **probability of winning the matchup**, not expected points. For a roughly normal score difference, win probability depends only on Z = (our mean - their mean) / sqrt(our variance + their variance), so anything that raises Z is good, and variance helps only when the mean difference is negative ([Skinner 2011](https://ar5iv.labs.arxiv.org/html/1111.0693)). **[sourced]**
- The variance effect is real but small next to the mean effect. An underdog at Z = -0.5 (31% to win) gains about 3 percentage points from a 20% wider spread, which is the same as adding about a tenth of a standard deviation of expected points. **[own calc]** Games played and player quality come first; variance is a tie-breaker between near-equal options.
- H2H records are noisy. In one six-season fantasy football league, actual win percentage differed from all-play win percentage by about plus or minus 22 points at the extremes, roughly plus or minus 3 wins in a 14-game season ([Elhabr 2023](https://tonyelhabr.rbind.io/posts/fantasy-football-performance/)). **[sourced, football, one league]**
- Judge team strength by **points scored** and all-play record, not by win-loss record ([Harstad, Footballguys](https://www.footballguys.com/article/HarstadFiT7)). **[sourced, football]**
- A 1-5 start is serious but its meaning depends on the playoff cut. In a 12-team dynasty football sample with 6 playoff spots, a 1-4 start made the playoffs about 9% of the time ([Parsons, Footballguys 2026](https://www.footballguys.com/article/2026-the-new-reality-no256-contender-or-pretender)). **[sourced, football]** Our own simulation of a 14-team league puts 1-5 at roughly 2-6% with 4 playoff spots, 5-13% with 6, and 14-23% with 8. **[own calc]**
- Those odds rise sharply if the team is actually good: a true top-four roster that started 1-5 still makes a 6-team playoff about 30-43% of the time in the same simulation. **[own calc]** So the first job at 1-5 is diagnosis: unlucky or weak?
- ESPN breaks seeding ties in H2H points leagues by head-to-head record among tied teams and then by **points for** ([ESPN support](https://support.espn.com/hc/en-us/articles/360058038711)). **[sourced]** Points for is therefore a real secondary objective, not just a vanity number.
- In hockey points leagues the goalie is the only position that can score negative points ([ESPN, Allen](https://africa.espn.com/fhl/story/_/id/35196481/espn-nhl-how-add-drop-stream-max-your-team)). **[sourced]** A goalie start is the main variance lever, and the main way to protect or chase a lead late in the week. **[inference]**
- Linemates' fantasy points are positively correlated, a goalie is negatively correlated with opposing skaters, and a goalie is roughly uncorrelated with his own skaters ([Hunter, Vielma and Zaman](https://ar5iv.arxiv.org/html/1604.01455)). **[sourced, DFS hockey]**
- With a limit of 2-3 moves per week, off-night streaming loses most of its edge and games-played quantity beats quality for replaceable players ([ESPN, Allen](https://africa.espn.com/fhl/story/_/id/35196481/espn-nhl-how-add-drop-stream-max-your-team); [DobberHockey, Brudner](https://dobberhockey.com/2023/04/11/eastern-edge-maximizing-the-end-of-the-fantasy-hockey-schedule)). **[sourced]**
- Fantasy playoffs are knockout, and "a 7-5 victory matters as much as a landslide" ([ESPN, Matiash 2026](https://africa.espn.com/fantasy/hockey/story/_/id/48184836/espn-nhl-fantasy-hockey-playoffs-h2h-league-tips-picks-strategies)). **[sourced]** The same Z rule applies, but future value drops to zero in a redraft league as elimination approaches. **[inference]**
- No published study was found that measures variance-seeking, stacking or defensive adds in season-long H2H hockey. The engine's own Monte Carlo is the right tool, and its recommendations need backtesting.

## 1. The season objective: weekly wins, playoff qualification, championship

**The chain.** The season prize goes to the bracket winner, so the season objective is P(championship) = P(qualify) x P(win each playoff round | qualify, seed). Weekly wins matter only through that chain. **[inference]**

**How much luck is in an H2H record.** An *all-play record* is the record a team would have if it played every other team every week; it removes schedule luck (who you happened to face) from the record. *Expected wins* is the same idea expressed as a sum: each week, the share of other teams you outscored ([csun.io](https://www.csun.io/2024/02/09/expected-wins-fantasy.html)). In a 10-team ESPN football league studied over 2018-2023, the top and bottom 2.5th percentiles of (actual win % minus all-play win %) were about plus or minus 22%, about plus or minus 3 wins in a 14-game season, and one manager was 16 points below his all-play percentage in a single season ([Elhabr](https://tonyelhabr.rbind.io/posts/fantasy-football-performance/)). That is one league, so treat the size as indicative. Harstad writes that "over a single league, the luck factor dominates" and models a record as .500 plus a skill factor plus a luck factor ([Footballguys](https://footballguys.com/article/HarstadADP)).

**Which measure of quality to trust.** Harstad argues that all-play record is a good description of who was lucky but a weaker measure of quality than raw points, because it ignores margins: a team can add many points without its all-play record changing. His advice is "if you want to know how good your team is, just check how much it's scored" ([Footballguys](https://www.footballguys.com/article/HarstadFiT7)). All-play and expected wins reward consistency; points for rewards size. **[sourced, football]** A related Footballguys series reports that point differential predicted later records better than win-loss record did, in NFL team samples **[snippet]**.

**How long until skill shows.** No source gave a number of weeks for season-long fantasy hockey. The MIT "Luck and the Law" study built a skill-versus-chance metric from FanDuel daily contests in 2013-14 and 2014-15 and found fantasy sports sit toward the skill end, with skill identified by persistence between half-seasons ([MIT DSpace record](https://dspace.mit.edu/handle/1721.1/119466); [theScore summary](https://www.thescore.com/mlb/news/1846139)). That concerns DFS players over many contests, not one H2H season. For intuition **[own calc]**: if two teams' weekly scores are normal with equal spread and the better team's mean is 0.5 team-standard-deviations higher, the better team wins a given week only about 64% of the time; at a gap of 0.25 it is 57%. Six weeks at those rates cannot separate a good team from an average one.

**Playoff odds after a bad start.** The only record-based table found is for dynasty football, 12 teams, 6 playoff spots: 1-2 gives 38%, 1-3 gives 24%, 1-4 gives 9%, 0-4 gives 2% ([Parsons](https://www.footballguys.com/article/2026-the-new-reality-no256-contender-or-pretender)). An analysis of over 1,000 ESPN and Sleeper football leagues found champions concentrate among the strongest regular-season teams, with the top two in its power ranking supplying about half of champions (28.2% and 22.6%) ([Fantasy Genius](https://fantasygenius.substack.com/p/what-are-my-odds-of-making-the-fantasy)). **[sourced, football]**

Our own simulation **[own calc]** used 14 teams, random weekly pairings, normal weekly scores, seeding by wins then points for, and three assumptions about how spread out true team strength is (standard deviation of team means equal to 0, 0.3 or 0.5 of one team's weekly standard deviation). Probability of making the playoffs given a 1-5 record after six weeks:

| Playoff spots | 20-week regular season | 23-week regular season | If the team is truly top-4 (20 to 23 weeks) |
|---|---|---|---|
| 4 of 14 | 1.5% to 4.7% | 1.9% to 5.7% | 13% to 19% |
| 6 of 14 | 5.1% to 10.9% | 6.3% to 12.8% | 30% to 43% |
| 8 of 14 | 13.7% to 21.3% | 15.1% to 23.4% | 49% to 68% |

The ranges run from "strength differs a lot between teams" (low end) to "all teams equal, pure luck" (high end). The last column uses the two skill settings only. Only 6-11% of 1-5 teams in the simulation were truly top-four. The regular-season length and number of playoff teams in the owner's leagues have since been confirmed (see the [README](README.md#league-facts-confirmed-from-the-app)): ESPN has 8 of 14 teams making the playoffs and a 24-week regular season (periods 1 to 24; 18 weeks left on 2026-11-10), and Fantrax has 6 of 12 teams. The relevant row for ESPN is therefore 8 of 14, and 24 weeks is one week beyond the 23-week column: about 15% to 24% for a 1-5 team (extrapolated from the table, not re-simulated). The engine's own estimate for this specific roster is about 12% ([09](09-engine-gap-analysis.md), item 10), below the table's range because the table assumes a team of average strength or better.

**Tiebreakers.** ESPN's standard seeding sequence for H2H points leagues is: head-to-head record among tied teams (only if they have played each other equally often), then points for, then intradivisional record, then points against, then a coin flip; division winners get the top seeds; a league manager can customise it ([ESPN support](https://support.espn.com/hc/en-us/articles/360058038711)). A tied playoff matchup goes to the higher seed by default ([ESPN support](https://support.espn.com/hc/en-us/articles/360000067252-Playoff-Tiebreakers)). Fantrax lets the commissioner choose among head-to-head record, division record, most wins, total points or random for standings, and highest seed, head-to-head record or home team for playoff ties ([FantraxHQ](https://fantraxhq.com/setting-up-your-fantrax-league-part-3/)), so the Fantrax league's own rules page is the authority.

## 2. The weekly objective: win probability, not expected points

**The theory.** Skinner's "Scoring Strategies for the Underdog" (Journal of Quantitative Analysis in Sports, 2011) treats a contest as two score distributions. With many independent scoring events the difference is close to normal, and P(win) is approximately 0.5 x [1 + erf(Z / sqrt 2)] with Z = (mu - mu_opp) / sqrt(sigma^2 + sigma_opp^2). "A team's optimum long-term strategy is that which maximizes Z." An underdog "must be willing to adopt a strategy whose outcome has a larger-than-average variance", even at some cost in mean, and "a favored team has an incentive to decrease the variance" ([Skinner](https://ar5iv.labs.arxiv.org/html/1111.0693)). The paper warns that the normal approximation is unreliable when few events remain, where an exact calculation is needed, and that it assumes independent events. **[sourced, real sports]**

**The exchange rate between mean and variance [inference from Skinner's formula].** Let D be the mean difference (ours minus theirs) and S the standard deviation of the difference. A change that moves our mean by dM and the variance of the difference by dV raises Z when dM > D x dV / (2 x S^2). If D is negative (underdog), extra variance is worth giving up some mean; if D is positive (favourite), extra variance has a cost. At D = 0 variance is irrelevant and only the mean matters.

**How big is it [own calc].** At Z = -0.5 the win probability is 30.9%; widening S by 10% gives 32.5%, by 20% gives 33.9%. At Z = -1.0, 15.9% becomes 20.2% with 20% more spread. A favourite at Z = +0.5 falls from 69.2% to 66.2% for the same widening. Raising the mean by 0.1 S moves an underdog at Z = -0.5 to about 34.5%, so a 20% change in spread is worth about a tenth of a standard deviation of mean. A weekly hockey total is the sum of something like 50-60 player-games, so one lineup choice changes S by far less than 20%. **[inference]** The practical conclusion is that variance matters most in lopsided matchups and late in the week, when few games remain and one decision is a large share of the remaining spread.

**Fantasy sources say the same thing qualitatively.** A Fantasy Footballers piece advises using "a few of your starting slots" on high-variance players when you are an underdog, and not going overboard: starting a boom/bust receiver over an elite one is wrong, over a similar-mean replacement it is right ([Larson 2020](https://www.thefantasyfootballers.com/articles/variance-in-fantasy-football-embrace-the-chaos/)). **[sourced, football]** numberFire notes that building a head-to-head lineup from 90th-percentile projections is not correct **[snippet]**. No source quantified the win-probability gain in a season-long league.

**How it shows up in hockey lineups.**

- *Games played.* More games raise both mean and variance, and the mean effect dominates. "Unless you are talking about players ranked among the top 100, quantity will usually beat quality" ([ESPN, Allen](https://africa.espn.com/fhl/story/_/id/35196481/espn-nhl-how-add-drop-stream-max-your-team)); "quantity is better than quality when it comes to streamer-level skaters" ([DobberHockey](https://dobberhockey.com/2023/04/11/eastern-edge-maximizing-the-end-of-the-fantasy-hockey-schedule)). **[sourced]**
- *Goalie starts.* ESPN default scoring is win 4, overtime loss 1, shutout 3, save 0.2, goal against -2; under it a 28-save 4-2 win is +5.6 and a five-goal loss on 31 shots is -4.8 ([ESPN, Matiash 2025](https://africa.espn.com/fhl/story/_/id/46216225/espn-fantasy-hockey-goalie-draft-stars-sleepers-best-picks)). Goalies "are the one roster position that can actually reduce your fantasy totals", one bad night produced starts of -8.4, -8.0 and -7.0, and "being wrong about one goaltender can negate being right about three skaters" ([ESPN, Allen](https://africa.espn.com/fhl/story/_/id/35196481/espn-nhl-how-add-drop-stream-max-your-team)). **[sourced for the default scoring]** Our ESPN league does not use the default: it scores win 2, overtime loss 1, shutout 2, save 0.2, goal against -1.5. Under our scoring the same 28-save 4-2 win is 2 + 5.6 - 3 = +4.6 and the five-goal loss on 31 shots is 26 x 0.2 - 5 x 1.5 = 5.2 - 7.5 = -2.3, so the downside is about half as large as the default scoring suggests (-2.3 against -4.8) and the reward for a win is smaller (win 2, not 4). Break-even save percentage on shots alone is 1.5 / 1.7 = .882 (default scoring: 2 / 2.2 = .909). Adding win points, a goalie with a 40-50% win chance on 28 shots breaks even at about (42 - 2w) / 47.6 = .861 to .866 (w = 0.5 and 0.4), ignoring shutouts and overtime losses. A goalie start in our ESPN league is therefore still a wide bet but its mean is positive for an average goalie (.896 on 28 shots is +0.65 before win points); it is a negative-mean bet only for goalies projected below about .86 to .87. A favourite should start a goalie unless his expected points are near or below zero (a weak goalie in a bad matchup); an underdog should start marginal goalies. (Fantrax scoring is friendlier still: break-even .800.) **[inference]**
- *Boom/bust skaters.* Between two skaters with near-equal expected weekly points, prefer the wider one as an underdog and the narrower one as a favourite. **[inference from Skinner]**

## 3. Correlation and stacking

**What is measured.** In DraftKings hockey data, forwards on the same line are much more correlated than forwards on different lines of the same team; a goalie and opposing skaters are negatively correlated; a goalie and his own team's skaters are near zero. The authors built lineups with complete lines, used only first-power-play defencemen, and never paired a goalie with opposing skaters, because positive correlation raises lineup variance and negative correlation lowers it. Over 38 top-heavy contests in late 2015, lineups without stacking had the lowest profit margin ([Hunter, Vielma and Zaman, "Picking Winners"](https://ar5iv.arxiv.org/html/1604.01455)). The paper's text gives no correlation coefficients (they are in a figure), and no numeric hockey coefficients were found elsewhere. **[sourced, DFS]**

**Why DFS conclusions do not transfer directly.** DFS tournaments pay only the top fraction of thousands of entries, so the paper wants high variance for every entry. An H2H matchup is a two-player contest where variance helps only the underdog (section 2). The transferable part is the *direction* of the correlations, not "always stack". **[inference]**

**The H2H version [inference].** The variance of the difference is Var(ours) + Var(theirs) - 2 x Cov(ours, theirs). Three consequences:

1. Starting linemates or a whole power-play unit raises Var(ours): good for an underdog, mildly bad for a favourite.
2. Starting a goalie against your own skaters lowers Var(ours): a hedge that suits a favourite.
3. Covariance with the opponent matters as well. Starting teammates of the opponent's best players makes the two totals move together and shrinks the spread of the difference, which suits a favourite. Starting a goalie who faces the opponent's skaters makes the totals move in opposite directions and widens it, which suits an underdog.

**Does it matter in season-long?** Probably little for most weekly decisions, because rosters are fixed by a draft, each player has 3-4 games, and the choice set is small. A football simulation found a stacked quarterback and receiver had a 15.3% chance of a "boom" week against 12.0% unstacked **[snippet, football]**, which suggests the scale: noticeable in the tail, modest overall. Correlation should be in the simulator (so the win probability is right) before it is used as a decision lever. Ignoring positive correlation among linemates understates the spread, which overstates a favourite's win probability and understates an underdog's. **[inference]**

## 4. Opponent-aware decisions

**Model the opponent explicitly.** Haugh and Singal (Management Science, 2021) show that modelling opponents' selections, rather than only one's own score, changes the optimal DFS portfolio; their problem reduces to mean-variance optimisation against a stochastic benchmark, for both "double-up" contests (beat a threshold, the format closest to H2H) and top-heavy ones ([abstract](https://econpapers.repec.org/RePEc:inm:ormnsc:v:67:y:2021:i:1:p:72-92)). **[sourced abstract only, DFS]** In H2H the benchmark is one known roster with a known schedule, so the opponent's distribution can be simulated directly.

**Streaming against a specific opponent.** With move limits, "limits to roster moves eliminate the advantage of off-night streaming", so plan moves around the nights when a streamer will actually fit in the lineup; Tuesdays, Thursdays and Saturdays are the busy nights ([ESPN, Allen](https://africa.espn.com/fhl/story/_/id/35196481/espn-nhl-how-add-drop-stream-max-your-team); [Daily Faceoff, 26 Oct 2026](https://www.dailyfaceoff.com/news/fantasy-hockey-2026-27-weekly-strength-of-schedule-and-streaming-targets-week-5)). "Adding a streamer just to put him on your bench... is a waste of an add" ([DobberHockey](https://dobberhockey.com/2023/04/11/eastern-edge-maximizing-the-end-of-the-fantasy-hockey-schedule)). **[sourced]** Opponent-awareness adds: value each move by its change in P(win this week), which is largest when the matchup is close and near zero when it is already decided either way. **[inference]**

**Defensive adds (taking a player the opponent needs).** Community sources treat a single block as legal and a grey area of sportsmanship, repeated add-and-drop "churning" as poor form, and coordinated manipulation between teams as collusion, with league rules as the final authority **[snippets: Footballguys forum, Fansided, RotoBaller]**. With 2-3 moves a week a block has a high opportunity cost, so it is worth it only when the opponent's gain from the player is larger than our best alternative use of the move. **[inference]**

**End-of-week tactics.** In a points league a skater cannot lower the total (unless the league scores a negative stat such as plus/minus), so benching skaters never protects a lead; goalies can. On the last day, the choice to start a goalie should come from an exact comparison of P(win | start) and P(win | sit) over the remaining games, not the normal approximation, which Skinner shows is weakest when few events remain ([Skinner](https://ar5iv.labs.arxiv.org/html/1111.0693)). Leading comfortably: sit a goalie whose downside could flip the result. Trailing by more than the skaters can plausibly make up: start every goalie available. **[inference]** One conflict: ESPN's points-for tiebreaker rewards maximising points even in a decided week, so sitting a positive-expectation goalie to protect a lead costs a little seeding value.

## 5. Scarce resources across the season

The owner has 2 adds per matchup period (7 days) on ESPN and 3 moves per week on Fantrax. A move has two possible payoffs: a change in this week's win probability and a change in rest-of-season roster strength. They compete, because a streamer picked for this week's schedule is often dropped next week.

- *Future value shrinks with time.* A player added early helps for most of the year; one added late helps for a few weeks, and unspent budget scores nothing **[snippets: FantasyPros, 4for4, Baseball Prospectus, all on FAAB, the free-agent auction budget]**. Sources disagree on how aggressively to spend early (from "never more than 20% on one player" to "75-80% on a true breakout") **[snippets]**.
- *Do not drop real assets for a stream.* The main risk of streaming is that "a dropped player becomes valuable to someone else"; players outside the top 100 skaters are generally replaceable ([ESPN, Allen](https://africa.espn.com/fhl/story/_/id/35196481/espn-nhl-how-add-drop-stream-max-your-team)). **[sourced]**
- *The value of a win is not constant.* A win is worth its change in P(playoffs), which is small when a team is safely in or hopelessly out and large on the bubble. The Footballguys table shows the leverage: a 1-2 team sits at 38%, and its next game moves it to 52% with a win or 24% with a loss, while a 0-4 team moves very little whatever happens ([Parsons](https://www.footballguys.com/article/2026-the-new-reality-no256-contender-or-pretender)). A "must-win" week is one with high leverage, and that is when spending future value (dropping a stash, using waiver priority, overpaying in a trade) is justified. **[inference]**
- *Keeper value has real weight on Fantrax.* A rest-of-season-only valuation is wrong for a team's likely top three, whose keeper value must be in the objective; for the other roster spots, which are redrafted, rest-of-season value is the right measure. **[inference]**

## 6. Playoffs

**Format and timing.** ESPN's standard fantasy hockey playoffs in 2025-26 were a two-week first round (16-29 March 2026) and an 18-day championship (30 March-16 April 2026) ([ESPN, Matiash 2026](https://africa.espn.com/fantasy/hockey/story/_/id/48184836/espn-nhl-fantasy-hockey-playoffs-h2h-league-tips-picks-strategies)); 2024-25 used the same shape ([ESPN, Matiash 2025](https://www.espn.com.au/fhl/story/_/id/44244119/fantasy-hockey-playoffs-ultimate-strategy-guide)). Custom leagues can set different lengths per round and can reseed after round one ([ESPN](https://africa.espn.com/fhl/story/_/id/39738751/fantasy-hockey-custom-league-playoff-settings)). The 2026-27 NHL season has 84 games per team, an All-Star weekend on 5-6 February 2027, and every team playing on 10 April 2027 ([ESPN](https://www.espn.com/nhl/story/_/id/49376073/nhl-releases-expanded-84-game-schedule-2026-27-season)); 10 April is reported as the final day **[snippet]**. The owner's leagues are now known: ESPN playoffs are periods 25 to 27 (March 22 to April 10, 2027) with 8 of 14 teams qualifying; Fantrax playoffs start in period 25, run three rounds ending 2027-04-11, and 6 of 12 teams qualify (see the [README](README.md#league-facts-confirmed-from-the-app)). Fantrax round lengths and a possible bye for the top seeds are unverified.

**Schedule becomes the main lever.** "Hacking the schedule to maximum benefit is paramount during the do-or-die knockout playoff stage", the largest edge is filling the lineup on light nights, and carrying extra goalies gives flexibility ([ESPN, Matiash 2026](https://africa.espn.com/fantasy/hockey/story/_/id/48184836/espn-nhl-fantasy-hockey-playoffs-h2h-league-tips-picks-strategies)). Multi-week rounds mean games over 14-18 days, not 7, decide a matchup, and move limits apply per matchup period in ways that must be checked per league. **[sourced, limits unverified]**

**Rest and shutdowns.** Teams out of the race may shut players down and contenders may rest stars; teams fighting for a playoff spot play their best lineups; an injured-reserve slot is wasted on a player who will not return before the real playoffs ([ESPN 2025](https://www.espn.com.au/fhl/story/_/id/44244119/fantasy-hockey-playoffs-ultimate-strategy-guide); [ESPN 2026](https://africa.espn.com/fantasy/hockey/story/_/id/48184836/espn-nhl-fantasy-hockey-playoffs-h2h-league-tips-picks-strategies)). **[sourced]** Late-season projections need a games-played discount that depends on the NHL team's standing. **[inference]**

**Seeding and risk.** Champions come disproportionately from the strongest regular-season teams ([Fantasy Genius](https://fantasygenius.substack.com/p/what-are-my-odds-of-making-the-fantasy)), partly from byes and partly because they are better. Single elimination does not change the weekly rule (maximise P(win)), but in redraft it removes the future: in the final, everything not playing in the final period is worth nothing. A low seed is usually the underdog in every round, so its whole bracket calls for variance. **[inference]**

## 7. A team that is far behind

**Step one: bad luck or weak roster?** Compare record with points-for rank and all-play record. A team near the top in points with a poor record has been unlucky and should mostly stay the course; Harstad's example is a team third in points at 0-4 ([Footballguys](https://www.footballguys.com/article/HarstadFiT7)). Parsons notes the opposite case: the deeper the hole, the less of it is plausibly schedule luck ([Footballguys](https://www.footballguys.com/article/2026-the-new-reality-no256-contender-or-pretender)). Also check points left on the bench: Elhabr estimates about 20 points a week lost to suboptimal starts in his football league, a measure of lineup process ([Elhabr](https://tonyelhabr.rbind.io/posts/fantasy-football-performance/)). **[sourced, football]**

**Options.**

1. *Stay the course* if points-for is top-third: the simulation says such teams still qualify often when 6 or 8 spots exist. **[own calc]**
2. *Raise the mean through volume*: schedule-driven streaming and consolidating depth into better starters ("elite players are scarce, while depth is much more replaceable", [McNamara](https://www.footballguys.com/article/2023-decision-to-rebuilding)). **[sourced, football]**
3. *Raise variance* in weeks where the team is an underdog (section 2). This helps weekly odds by a few points; it does not rescue a weak roster. **[own calc]**
4. *Trade future for present* in redraft, where future value expires at season end, as long as the playoffs are still reachable. **[inference]**
5. *Sell for the future* in dynasty. "The dynasty rebuild is a declaration by a GM that a future season is a more optimistic time to win than the present one"; it "reduces your optionality" and done badly "can cost you years"; check all-play record and count elite players before deciding ([McNamara](https://www.footballguys.com/article/2023-decision-to-rebuilding)). **[sourced, football]**

## 8. Judging decisions independent of outcomes

"Resulting" is Annie Duke's term for judging a decision by its outcome; the remedy is to record the reasoning and probabilities before the outcome is known **[snippet; the book was not consulted]**. For the engine this becomes:

- *Calibration.* Log the predicted weekly win probability and score it against results (Brier score, reliability by bucket). With about 20-25 matchups per league per season the sample is small, so also score every *other* matchup in both leagues, which multiplies the sample. **[inference]**
- *Expected value accounting.* For each recommendation log the predicted change in P(win) and in projected points, then compare realised points of the chosen option against the rejected one. Elhabr's actual-minus-projected and lost-opportunity tables are a template ([Elhabr](https://tonyelhabr.rbind.io/posts/fantasy-football-performance/)). **[sourced method, football]**
- *Luck ledger.* Report all-play record and expected wins beside the real record each week so the owner sees schedule luck directly ([csun.io](https://www.csun.io/2024/02/09/expected-wins-fantasy.html)).
- *Backtesting.* Replay past weeks using only information available at decision time and compare the engine's policy with simple baselines (maximise expected points; maximise games played).

## 9. Published work and what transfers

- **Skinner 2011, JQAS**: variance for underdogs. Transfers directly as the weekly objective; the independence assumption does not hold between linemates. ([link](https://ar5iv.labs.arxiv.org/html/1111.0693))
- **Hunter, Vielma and Zaman, "Picking Winners"**: integer programming for DFS hockey with stacking. The correlation structure transfers; "maximise variance" does not. ([link](https://ar5iv.arxiv.org/html/1604.01455))
- **Haugh and Singal 2021, Management Science**: opponent modelling; the double-up case is the H2H analogue. ([link](https://econpapers.repec.org/RePEc:inm:ormnsc:v:67:y:2021:i:1:p:72-92))
- **Becker and Sun 2016, JQAS**: a mixed-integer model for a whole fantasy football season, draft plus weekly lineups, whose stated goal is winning the season, not maximising weekly scores. The abstract reports only "promising" tests; the full text was not read. ([link](https://econpapers.repec.org/RePEc:bpj:jqsprt:v:12:y:2016:i:1:p:17-30:n:1))
- **Getty, Li, Yano, Gao and Hosoi 2018, SIAM Review**: a skill-versus-luck metric for fantasy contests. ([link](https://dspace.mit.edu/handle/1721.1/119466))

No peer-reviewed work on season-long H2H fantasy hockey strategy was found.

## How this applies to our two leagues

**ESPN, 14 teams, redraft, 1-5.**

1. The league settings are now known: 8 of 14 teams make the playoffs and 18 regular-season weeks were left on 2026-11-10 (a 24-week season), so the relevant odds for a 1-5 team are the 8-spot row, about 15-24% (14-23% in the 20 to 23-week columns; extrapolated to 24 weeks), and the engine says about 12% for this roster. The settings break standings ties on points for (`playoffSeedingRule` `TOTAL_POINTS_SCORED`), which `fm outlook` now uses. **[own calc]**
2. Compute points-for rank, all-play record and expected wins. Top-five in points means unlucky: keep the core and make no panic trades. Bottom-half means the roster is the problem.
3. Either way the remaining weeks are high-leverage, and this is redraft, so future value beyond this season is zero. Spend both weekly moves on games played in lineup slots that would otherwise be empty, and trade depth or long-term "stash" players for current production. **[inference]**
4. Chase points for in every week, including lost ones, because it is the second seeding tiebreaker ([ESPN support](https://support.espn.com/hc/en-us/articles/360058038711)).
5. Use variance only as a tie-breaker in underdog weeks: marginal goalie starts, linemate stacks, players uncorrelated with the opponent's.
6. Set a stop rule: when simulated playoff odds fall below a small threshold, the remaining objective in a redraft league is enjoyment and league integrity; keep setting honest lineups. **[inference]**

**Fantrax, 12 teams, keep 3 per team (labelled Dynasty), 6 of 12 make the playoffs, 3-3.**

1. A 3-3 team is on the bubble, where each win has the highest leverage. Do not decide contend-or-rebuild from the record; use points-for rank, all-play record and the count of elite players ([McNamara](https://www.footballguys.com/article/2023-decision-to-rebuilding)).
2. The objective is a weighted sum of this season's title odds and the future strength of the three kept players (only the top 3 per team carry over; everyone else is redrafted). Selling early "reduces your optionality", so hold the decision until the points-for evidence is clearer or the league's trade deadline forces it. **[sourced principle, football]**
3. Use the 3 weekly moves for schedule volume, but never drop a likely top-3 keeper for a one-week stream.
4. If the roster is top-third in points, buy; if bottom-third, sell veterans while they are productive.

## Implications for the engine

Well supported by sources:

1. **Lineup choice maximises P(win this week)** given the opponent's simulated distribution, not expected points. The two agree when the matchup is even and diverge as it becomes lopsided ([Skinner](https://ar5iv.labs.arxiv.org/html/1111.0693)). *Test:* in a synthetic matchup where option A has a slightly higher mean and option B a wider spread, the engine picks B when a clear underdog and A when a clear favourite.
2. **The simulator must model correlation**: positive within a line and power-play unit, negative between a goalie and opposing skaters, near zero between a goalie and his own skaters ([Hunter et al.](https://ar5iv.arxiv.org/html/1604.01455)). *Test:* simulated weekly team variance matches the historical variance of weekly totals; an independence-only simulation should understate it.
3. **Goalie starts are evaluated with the full distribution, including negative outcomes**, under the league's actual scoring ([ESPN](https://africa.espn.com/fhl/story/_/id/35196481/espn-nhl-how-add-drop-stream-max-your-team)).
4. **Team strength is reported as points-for rank, all-play record and expected wins**, beside the real record ([Harstad](https://www.footballguys.com/article/HarstadFiT7); [csun.io](https://www.csun.io/2024/02/09/expected-wins-fantasy.html)).
5. **Streaming value counts only games that fit in the lineup**, night by night ([DobberHockey](https://dobberhockey.com/2023/04/11/eastern-edge-maximizing-the-end-of-the-fantasy-hockey-schedule)).
6. **Season simulation uses the league's real tiebreakers**; on ESPN that puts points for directly into seeding ([ESPN support](https://support.espn.com/hc/en-us/articles/360058038711)).

Our own inference (to be validated by backtest):

7. **Weekly moves maximise change in P(playoffs) (later, P(title)), not change in P(win this week)**: value = leverage of this week x change in P(win) + change in rest-of-season strength, with the second term discounted to zero at elimination in redraft and extended across seasons in dynasty.
8. **Late-week decisions use exact enumeration or simulation over remaining games**, not a normal approximation.
9. **Opponent covariance is a lever**: report how correlated our lineup is with the opponent's and prefer low or negative covariance as an underdog, high as a favourite.
10. **Trades are scored on the season objective**, so the same trade can be right for the 1-5 redraft team and wrong for the dynasty team.
11. **A defensive add is recommended only when** the opponent's loss in P(win) exceeds the gain from our best ordinary move, and only within league rules.
12. **A decision log** stores every recommendation with predicted probabilities, so calibration and counterfactual value can be computed.
13. **Variance preference never overrides a clearly better mean.** The exchange rate dM > D x dV / (2 S^2) bounds how much mean an underdog may give up.

## Open questions / where sources disagree

- **Points for versus all-play as the quality measure.** Harstad prefers points for; the all-play and expected-wins authors prefer consistency-rewarding measures. Neither was tested for predictive power in the sources read. The engine can test this on its own league history.
- **How much variance-seeking is worth in season-long H2H.** All fantasy sources are qualitative. Our calculation says a few percentage points at most.
- **Hockey correlation sizes.** Direction is sourced; magnitudes are not, and they depend on scoring. They must be estimated from game logs.
- **How spread out team strength is in these leagues** (the 0, 0.3, 0.5 assumption). This drives the 1-5 odds and can be estimated from weekly scores.
- **FAAB and move timing.** Practitioner sources disagree on spending early versus saving.
- **Playoff odds by record for hockey leagues.** Only football tables were found.
- **Rebuild timing in dynasty hockey.** The criteria found are football-specific.
- **Ethics of blocking.** Norms vary by league.

## Sources

Read for this document:

- https://ar5iv.labs.arxiv.org/html/1111.0693 (Skinner, Scoring Strategies for the Underdog)
- https://ar5iv.arxiv.org/html/1604.01455 (Hunter, Vielma, Zaman, Picking Winners in Daily Fantasy Sports Using Integer Programming)
- https://econpapers.repec.org/RePEc:inm:ormnsc:v:67:y:2021:i:1:p:72-92 (Haugh and Singal, abstract only)
- https://econpapers.repec.org/RePEc:bpj:jqsprt:v:12:y:2016:i:1:p:17-30:n:1 (Becker and Sun, abstract only)
- https://dspace.mit.edu/handle/1721.1/119466 (Luck and the Law, repository record and abstract)
- https://www.thescore.com/mlb/news/1846139 (news summary of Luck and the Law)
- https://tonyelhabr.rbind.io/posts/fantasy-football-performance/
- https://www.footballguys.com/article/HarstadFiT7
- https://footballguys.com/article/HarstadADP
- https://www.csun.io/2024/02/09/expected-wins-fantasy.html
- https://fantasygenius.substack.com/p/what-are-my-odds-of-making-the-fantasy
- https://www.footballguys.com/article/2026-the-new-reality-no256-contender-or-pretender (free portion)
- https://www.footballguys.com/article/2023-decision-to-rebuilding
- https://www.thefantasyfootballers.com/articles/variance-in-fantasy-football-embrace-the-chaos/
- https://support.espn.com/hc/en-us/articles/360058038711
- https://support.espn.com/hc/en-us/articles/360000067252-Playoff-Tiebreakers
- https://africa.espn.com/fhl/story/_/id/35196481/espn-nhl-how-add-drop-stream-max-your-team
- https://africa.espn.com/fhl/story/_/id/46216225/espn-fantasy-hockey-goalie-draft-stars-sleepers-best-picks
- https://www.espn.com.au/fhl/story/_/id/44244119/fantasy-hockey-playoffs-ultimate-strategy-guide
- https://africa.espn.com/fantasy/hockey/story/_/id/48184836/espn-nhl-fantasy-hockey-playoffs-h2h-league-tips-picks-strategies
- https://africa.espn.com/fhl/story/_/id/39738751/fantasy-hockey-custom-league-playoff-settings
- https://www.espn.com/nhl/story/_/id/49376073/nhl-releases-expanded-84-game-schedule-2026-27-season
- https://dobberhockey.com/2023/04/11/eastern-edge-maximizing-the-end-of-the-fantasy-hockey-schedule
- https://www.dailyfaceoff.com/news/fantasy-hockey-2026-27-weekly-strength-of-schedule-and-streaming-targets-week-5
- https://fantraxhq.com/setting-up-your-fantrax-league-part-3/

Seen only as search-result summaries (claims marked [snippet]; open before relying on them):

- https://www.footballguys.com/article/2020-regression-alert-week-11 (point differential versus record)
- https://www.thefantasyfootballers.com/articles/stick-to-stacking-exploring-the-classic-boom-bust-strategy-fantasy-football/ (stack boom rates)
- https://www.thescore.com/nhl/news/3566950/nhl-unveils-2026-27-schedule (season end date)
- https://www.numberfire.com/nfl/news/41450/daily-fantasy-football-floor-and-ceiling-projections-week-14 (ceiling projections and head-to-head lineups)
- https://www.fantasypros.com/2024/08/faab-waiver-wire-strategy-advice-fantasy-football/amp/ and https://www.4for4.com/2025/preseason/ultimate-guide-waiver-wire-faab-strategy-2025 (FAAB timing)
- https://forums.footballguys.com/threads/blocking-your-opponent-thoughts.807476 and https://fansided.com/2019/11/06/fantasy-football-waiver-wire-block-teams/ (blocking norms)
- https://www.osam.com/pdfs/Annie_Duke_Transcript.pdf (resulting)

Could not be opened (HTTP 403 or 429): the SSRN and INFORMS pages for Haugh and Singal, the full PDF of Luck and the Law, and a Big Think article on outcome-blind analysis.
