# 03 - What drives a hockey player's fantasy value

Researched 2026-11-10. The last complete NHL season at the time of writing is 2025-26 (82 games per team); 2026-27 is an 84-game season according to [Daily Faceoff](https://www.dailyfaceoff.com/news/2026-27-fantasy-hockey-strength-of-schedule).

## What this covers

This document explains what makes a skater or goalie produce fantasy points: opportunity (ice time and power-play role), the individual statistics that are scored, how repeatable each one is, how goalie value works, and how schedule, injuries and position eligibility change value. It then works through what our two head-to-head points leagues reward. Two kinds of evidence are used and are kept apart: (a) published sources, linked inline, and (b) **"own calculation"** figures that I computed on 2026-11-10 from the NHL's public statistics API ([skater summary](https://api.nhle.com/stats/rest/en/skater/summary?limit=-1&cayenneExp=seasonId=20252026%20and%20gameTypeId=2), [skater realtime](https://api.nhle.com/stats/rest/en/skater/realtime?limit=-1&cayenneExp=seasonId=20252026%20and%20gameTypeId=2), [skater time on ice](https://api.nhle.com/stats/rest/en/skater/timeonice?limit=-1&cayenneExp=seasonId=20252026%20and%20gameTypeId=2), [scoring per game](https://api.nhle.com/stats/rest/en/skater/scoringpergame?limit=-1&cayenneExp=seasonId=20252026%20and%20gameTypeId=2), [goalie summary](https://api.nhle.com/stats/rest/en/goalie/summary?limit=-1&cayenneExp=seasonId=20252026%20and%20gameTypeId=2), [goalie days rest](https://api.nhle.com/stats/rest/en/goalie/daysrest?limit=-1&cayenneExp=seasonId=20252026%20and%20gameTypeId=2), [team summary](https://api.nhle.com/stats/rest/en/team/summary?limit=-1&cayenneExp=seasonId=20252026%20and%20gameTypeId=2), [team power-play time](https://api.nhle.com/stats/rest/en/team/powerplaytime?limit=-1&cayenneExp=seasonId=20252026%20and%20gameTypeId=2)) for seasons 2023-24 to 2025-26. Own calculations are simple and reproducible but have not been peer reviewed; the engine should re-derive them rather than hard-code them.

Limits of the own calculations: the fantasy-point totals leave out ESPN's hat-trick bonus (HAT) and "DEF 0.1", and Fantrax's "FT 3", because I could not confirm what the last two measure. Everything else in both scoring systems is included.

Correction added after the research (from the app's own code, see [README](README.md#league-facts-confirmed-from-the-app)): Fantrax "FT" is fights (fighting majors), worth 3 points each, so the Fantrax totals here understate enforcers. ESPN "DEF" is ESPN's stat 33, which we read as defensemen points (0.1 extra per point scored by a defenseman), so the ESPN totals slightly understate scoring defensemen.

## Key takeaways

- **Opportunity is the most stable thing about a skater.** Ice time per game and power-play ice time per game correlate about 0.8 to 0.9 from one season to the next, and points per game correlates 0.87 with ice time among forwards in the same season (own calculation).
- **The first power-play unit is where power-play points are.** In 2025-26 the five most-used power-play skaters on each team averaged about 23 power-play points per 82 games; the next five averaged about 7 (own calculation).
- **Volume statistics repeat; percentages do not.** Shots, hits, blocks, penalty minutes and faceoff wins per game correlate 0.72 to 0.94 year to year. Shooting percentage correlates only 0.22 to 0.38 (own calculation), which matches the published finding that high shooting percentages rarely repeat ([JFresh](https://jfresh.substack.com/p/percentage-luck-in-hockey-explained)).
- **Our ESPN league is a blocks league for defensemen.** A block is worth half an assist, and 45% of the fantasy points of the top 80 defensemen come from blocks (own calculation).
- **Our Fantrax league is a shot-volume league with a physical bonus.** A shot is worth one eighth of a goal (against one twentieth on ESPN), and hits and penalty minutes add meaningful points; 26% of top forwards' points come from shots alone (own calculation).
- **Goalie save percentage is close to unpredictable.** Year-to-year correlation was 0.12 to 0.15 for goalies with 20+ starts in both seasons (own calculation). Starts and shots faced are far more predictable.
- **The two leagues want different goalies.** On ESPN (saves +0.2, goals against -1.5) a goalie breaks even on save/goal points at a .882 save percentage, so quality decides value and bad goalies are worth close to zero. On Fantrax (saves +0.25, goals against -1) break-even is .800, so every start is strongly positive and volume decides value (own arithmetic).
- **Wins belong to the team more than the goalie.** Wins per start correlated 0.77 with team win rate and only 0.45 with the goalie's own save percentage in 2025-26 (own calculation).
- **Workhorse goalies are nearly gone.** One goalie reached 60 starts in 2025-26; the median team's top goalie started 62% of games (own calculation).
- **Goalies almost never play on zero days' rest.** About 2% of goalie appearances in each of the last three seasons came the day after another appearance (own calculation), so on a back-to-back the engine should assume a split.
- **Hits and blocks are recorded by home-arena scorers and are biased by rink** ([Schuckers and Macdonald](https://ar5iv.arxiv.org/html/1412.1035)).
- **Schedule is a tiebreaker, not a driver**, and matters most for the fringe of the roster ([Daily Faceoff](https://www.dailyfaceoff.com/news/fantasy-hockey-2024-25-using-light-days-to-your-advantage-on-draft-day)).
- **Centre/wing eligibility is irrelevant in both leagues** because both use generic forward (F) slots; only forward versus defense matters (inference from the roster settings).

## 1. Skater opportunity

### Time on ice

Time on ice (TOI) is the minutes a skater plays per game. DobberHockey's rule of thumb is that forwards scoring a point per game usually play at least 18:00, and that only 22 of 308 past seasons at a 0.75 points-per-game pace came from forwards under 17:00 ([DobberHockey](https://dobberhockey.com/2021/02/10/goldipucks-and-the-three-skaters-crunching-the-numbers)). Own calculation for 2025-26, skaters with 40+ games:

| Forward TOI per game | Players | Points/game | ESPN pts/game | Fantrax pts/game |
|---|---|---|---|---|
| under 12 min | 77 | 0.21 | 0.83 | 2.17 |
| 14-16 | 81 | 0.46 | 1.21 | 3.00 |
| 16-18 | 80 | 0.62 | 1.52 | 3.77 |
| 18-20 | 63 | 0.82 | 1.87 | 4.52 |
| 20+ | 33 | 1.11 | 2.37 | 5.71 |

For defensemen the same pattern holds (0.24 points/game at 16-18 minutes, 0.78 at 24+), and blocks per game rise from 1.0 to 1.6 across the range. These are associations: coaches give more minutes to better players, so TOI partly measures talent. For the engine that does not matter much, because TOI is observable daily and is itself stable (year-to-year correlation 0.87 for forwards, 0.80 for defensemen).

### Power-play unit and share of power-play time

PP1 is a team's first power-play unit (the five skaters who start most power plays); PP2 is the second unit. "PP share" is the fraction of the team's power-play minutes a player is on the ice for. DobberHockey recommends measuring share rather than raw minutes because teams draw penalties at different rates ([DobberHockey](https://dobberhockey.com/2021/02/10/goldipucks-and-the-three-skaters-crunching-the-numbers)). Reported shares for individual top-unit players in early 2024-25 ranged from roughly 60% on teams that split units (Calgary, Los Angeles) to 75-85% on teams that load one unit (Minnesota, Nashville, Vegas, Edmonton) ([DobberHockey](https://dobberhockey.com/2024/11/04/the-wild-west-power-play-changes)).

Own calculation for 2025-26 (single-team skaters, share measured per game played): the most-used power-play skater on a team had a median share of 72% (range 57% to 92%), the fifth most-used 56%, and the sixth to tenth averaged 32%. Production by share:

| PP share | Players | Power-play points per 82 games |
|---|---|---|
| under 10% | 199 | 0.2 |
| 25-40% (typical PP2) | 84 | 6.6 |
| 40-55% | 51 | 11.6 |
| 55-70% | 88 | 21.0 |
| 70%+ | 49 | 29.7 |

Power-play points (PPP) made up about a third of all points for forwards playing 2.5+ power-play minutes a game and for defensemen in the same range. For defensemen, points per game correlated more strongly with power-play ice time (0.87) than with total ice time (0.73): the one defenseman on PP1 is usually the only high-scoring defenseman on the team.

Two cautions from the sources. First, heavy power-play time on a weak power play is not worth much; ESPN's column on power-play efficiency makes this point (found in search results only; I did not read the article in full). Team power-play success ranged from 16% to 31% and team power-play goals from 37 to 71 in 2025-26 (own calculation). Second, power-play scoring *rate* is noisy: points per 60 power-play minutes correlated only about 0.45 year to year for forwards, while power-play *minutes* correlated about 0.9 (own calculation). The role is the signal; a hot or cold conversion rate mostly is not.

### Linemates and even-strength line

Hockey Graphs found that a one-point rise in the shot share of a player's teammates moved his own results by about 1.5 points, the same size as (and opposite to) the effect of competition, but that the spread in teammate quality across players is about five times the spread in competition quality ([Hockey Graphs](https://hockey-graphs.com/2015/10/08/why-linemate-and-competition-metrics-may-not-be-as-simple-as-we-think/)). In plain terms: who you play *with* varies a lot and matters; who you play *against* evens out. A promotion to a line with an elite centre is therefore a real value change; opponent matchups are mostly noise over a week.

### Zone starts

Offensive-zone start percentage is the share of a player's faceoff-started shifts that begin in the attacking zone. DobberHockey notes that only one of the top 30 scorers in 2021-22 had a share below 50% and the group averaged 58.6% ([DobberHockey](https://dobberhockey.com/2022/11/21/wild-west-why-zone-starts-are-more-important-than-you-think)). Analysts disagree on whether this is cause or effect: other work argues most shifts start on the fly and that zone starts alone account for only a couple of points a season (search-result summaries of work by Matt Cane and David Johnson; not read in full). Treat zone starts as weak supporting evidence of an offensive role, not as an input worth much weight.

### Team scoring environment

Teams scored between 2.54 and 3.63 goals per game in 2025-26, a 43% spread between the lowest and highest (own calculation). A player's point ceiling scales with how many goals are scored while he is on the ice, so the same role is worth more on a high-scoring team.

### How fast a role change shows up

I found no rigorous study of this. What the data supports: role (TOI, PP share) is the stable, observable variable and conversion is the noisy one, so a role change should move the projection immediately, in proportion to the minutes gained, rather than waiting for points to appear. DobberHockey advises judging power-play usage on recent stretches rather than season totals because roles change during the season, and its unit trackers use a five-game window (search-result summary of DobberHockey columns; treat the window length as convention, not evidence). **Inference:** weight the last 5-10 games heavily for *deployment* and the last one to three seasons for *rates*.

### Reading line combinations and source reliability

A line-combination page lists each team's forward lines (three forwards each), defense pairs, and the two power-play units. Daily Faceoff describes itself as a news site with no affiliation to the NHL and, as far as my searches found, does not publish an accuracy record or a description of how its lines are gathered ([Daily Faceoff](https://www.dailyfaceoff.com/team-line-combinations/); search-result summary only, the page itself was not read). **I could not find any audit of how often its projected lines match what is actually iced**, so their accuracy is unverified. Coaches also shuffle lines during games. The practical check is the NHL's own data: actual TOI and power-play TOI from the last few games show whether a listed role is real. Daily Faceoff's starting-goalie page is better documented: each entry is labelled "Confirmed" or "Unconfirmed", and confirmed entries carry a timestamp and a named source ([Daily Faceoff](https://www.dailyfaceoff.com/starting-goalies)).

## 2. Skater production components

### What repeats and what does not

Own calculation: Pearson correlation between one season and the next, players with 40+ games in both seasons, two season pairs (2023-24 to 2024-25, 2024-25 to 2025-26); about 335 forwards and 163 defensemen per pair.

| Per-game statistic | Forwards | Defensemen |
|---|---|---|
| Faceoff wins | 0.93-0.94 | not applicable |
| Shots on goal | 0.89 | 0.84-0.87 |
| Hits | 0.89-0.91 | 0.87-0.89 |
| Assists | 0.82-0.84 | 0.79-0.80 |
| Power-play points | 0.82-0.84 | 0.83-0.85 |
| Primary assists | 0.78-0.79 | 0.69-0.70 |
| Penalty minutes | 0.75-0.78 | 0.72-0.74 |
| Goals | 0.74-0.75 | 0.60-0.71 |
| Blocks | 0.72-0.76 | 0.73-0.75 |
| Secondary assists | 0.64-0.69 | 0.69-0.72 |
| Shooting percentage | 0.36-0.38 | 0.22-0.26 |
| ESPN fantasy points | 0.84 | 0.79-0.82 |
| Fantrax fantasy points | 0.85 | 0.81-0.85 |

These correlations are across the whole league, so they are driven largely by role (a fourth-liner and a star stay far apart). They overstate how well small differences between similar players persist.

- **Goals** are shots times shooting percentage. Shot volume repeats; shooting percentage mostly does not. JFresh found that of 37 players who shot 15% or better at five-on-five in one season, only about four would be expected to repeat, and that a player's on-ice shooting percentage (the rate at which his team scores while he is on the ice) repeats even less: 3.6% of players above 11% did it again ([JFresh](https://jfresh.substack.com/p/percentage-luck-in-hockey-explained)). MoneyPuck treats "shooting talent", the ability to score more than an average player on the same chances, as a real but separate quantity from expected goals ([MoneyPuck glossary](https://moneypuck.com/glossary.htm)).
- **Assists.** A primary assist is the last pass before a goal; a secondary assist is the pass before that. Fantasy writers treat a high secondary share as luck: DobberHockey flags forwards above roughly 33% and defensemen above roughly 50% as running hot ([DobberHockey](https://dobberhockey.com/2021/02/10/goldipucks-and-the-three-skaters-crunching-the-numbers)). League-wide in 2025-26, 45% of assists were secondary (6,082 of 13,655; own calculation). My data shows secondary assists are less repeatable than primary for forwards but not for defensemen, so the "secondary assists are luck" rule is weaker than usually stated. Hockey Graphs found that a player's rate of shots plus passes that lead to shots predicted future primary points better than past primary points did, on a 72-forward sample ([Hockey Graphs](https://hockey-graphs.com/2016/01/27/redefining-shot-quality-one-pass-at-a-time/)).
- **IPP** (individual points percentage) is the share of goals scored with a player on the ice on which he gets a point. DobberHockey uses 70% as the level a top forward usually exceeds and reads a figure far from the player's norm as temporary ([DobberHockey](https://dobberhockey.com/2021/02/10/goldipucks-and-the-three-skaters-crunching-the-numbers)).
- **Hits, blocks, penalty minutes, faceoffs** are style and role statistics and repeat well. Two caveats. League-wide levels shift: recorded blocks jumped 34.5% in 2023-24 while hits fell 9.1% ([DobberHockey](https://dobberhockey.com/2024/07/23/ramblings-changes-in-shots-hits-blocks-and-pims-in-recent-seasons-july-23)); my 2025-26 totals are 37,236 blocks and 53,658 hits. And they are recorded by scorers in the home arena: over 2007-13, New Jersey's rink recorded blocks at 0.54 times and hits at 0.59 times the league rate, Montreal's blocks at 1.27 times, and home teams got about 11% more hits and 5% more blocks than visitors ([Schuckers and Macdonald](https://ar5iv.arxiv.org/html/1412.1035)). That study is old; whether the same rinks are biased today is unverified.

### The "bangers" effect

A "banger" is a player whose value comes from hits, blocks and penalty minutes rather than scoring; a "banger league" scores those. Published guidance: defenseman rankings depend heavily on whether blocks and hits count (The Hockey News, search-result summary only); DobberHockey's z-score analysis found heavy-hitting defensemen worth rostering when hits, blocks and penalty minutes all count but not when only hits count ([DobberHockey](https://dobberhockey.com/2024/11/17/geek-of-the-week-peripheral-category-values/)); ESPN's Sean Allen showed that on ESPN scoring a defenseman can earn 1.7 to 2.2 fantasy points a game from blocks, hits and shots alone ([ESPN](https://africa.espn.com/fhl/story/_/id/34823209/espn-fantasy-hockey-let-talk-hits-blocks-which-players-get-both)).

**Sources disagree about what a block is "really" worth.** Daily Faceoff's regression of team statistics on wins values a block at slightly negative and a hit near zero, because teams that block and hit a lot tend not to have the puck ([Daily Faceoff](https://www.dailyfaceoff.com/news/using-analytics-to-maximize-your-fantasy-hockey-leagues-scoring-system)). That is about real hockey. For fantasy valuation only the league's scoring matters, and our leagues pay for blocks and hits.

## 3. Goalie value

### Workload

A goalie's season value is starts times points per start. Starters used to play 68-70 games; teams now split closer to 50-50 in "1A/1B" tandems (two goalies sharing the net with a slight lean to one) ([Daily Faceoff](https://dailyfaceoff.com/news/backup-goalies-nhl-more-important-reliable-quality-starts-charlie-lindgren-casey-desmith)). Own calculation: goalies with 60+ starts numbered 3, 5 and 1 in the last three seasons; with 50+ starts, 14, 20 and 18; the maximum was 63-64. In 2025-26 the median team's top goalie started 62% of games (middle half of teams: 52% to 67%; maximum 77%), and on 11 of 32 teams nobody reached 55%.

Starts are moderately predictable from last season (correlation 0.55 for goalies with 20+ starts both years) and shots faced per game more so (0.38-0.56), because shots faced track the team: a goalie's shots against per 60 minutes correlated 0.88 with his team's shots against per game (own calculation). Teams allowed between 23.9 and 32.4 shots per game in 2025-26.

### Save percentage is mostly noise over a season

Save percentage is saves divided by shots faced. Hockey Graphs calls it repeatable only "once sufficient sample size is met" and cites roughly 1,500 shots at even strength as a point where it starts to reflect skill; that is more than most goalies face in a season ([Hockey Graphs](https://hockey-graphs.com/2014/01/20/2013-2014-goaltending-performance-save-percentage-correlation/)). JFresh describes team save percentage as having "weak repeatability" ([JFresh](https://jfresh.substack.com/p/percentage-luck-in-hockey-explained)). Own calculation: year-to-year correlation of save percentage was 0.12 and 0.15 for goalies with 20+ starts in both seasons, and 0.24 and 0.33 for the roughly 20 goalies with 40+ starts in both. The league average has also drifted down: .9035, .9001, .8958 over the last three seasons. Goals saved above expected (expected goals against minus actual goals against, per [MoneyPuck](https://moneypuck.com/glossary.htm)) adjusts for shot difficulty and is a better talent measure than raw save percentage, though I did not find a verified repeatability figure for it.

### Wins, shutouts, quality starts

Wins depend mainly on the team: wins per start correlated 0.77 with team win rate and 0.45 with the goalie's own save percentage, and wins per start barely repeated year to year (0.01 to 0.33) (own calculation). Shutouts occurred in 4.2% to 5.8% of starts league-wide (own calculation), so they are rare and should be projected from the league rate adjusted for team defence, not from a goalie's own history. A "quality start" (Rob Vollman's statistic) is a start with a save percentage at or above the league average, with a looser threshold of .885 in low-shot games; Hockey-Reference's rule of thumb is that under 50% is bad and over 60% elite (search-result summary of the [Sports Reference blog](https://www.sports-reference.com/blog/2014/12/games-started-quality-starts-on-hockey-reference-com/); the exact low-shot rule is unverified).

### Back-to-backs and how starters are chosen

A back-to-back is two games on consecutive days. Eric Tulsky's 2013 study found that goalies playing both nights did measurably worse on the second; NHL.com reports it as a drop of "just over 11 percent" (other summaries give 90.1% against 91.2%, about one percentage point, which is the likelier reading), and notes that later work by Andrew Thomas and Dom Luszczyszyn found a smaller effect ([NHL.com](https://nhl.com/kraken/news/core-concepts-examining-goalie-workload-329507592)). A Hockey Graphs study of about 40,000 games found save percentage 8 points lower (.897 against .905) and win rate much lower (.421 against .498) in the second game of a back-to-back, with the author warning that most such games are on the road ([Hockey Graphs](https://hockey-graphs.com/2014/12/01/goaltender-performance-vs-rest/)). Coaches have responded: Seattle's coach said "typically you're going to have a split on the back-to-backs" ([NHL.com](https://nhl.com/kraken/news/core-concepts-examining-goalie-workload-329507592)). Own calculation: only 1.5% to 2.1% of goalie appearances in the last three seasons came on zero days' rest, and goalies with 45+ starts averaged about one such appearance a season (maximum four). Those appearances include relief appearances, so the rate of starting both games is lower still.

Other patterns reported by fantasy writers, without counts: the backup typically gets the second night of a back-to-back, the last game of a long road trip and afternoon games (theScore, search-result summary only). Most teams are reported to announce the starter after the morning skate on game day (search-result summary only); Daily Faceoff's page shows which starters are confirmed and when ([Daily Faceoff](https://www.dailyfaceoff.com/starting-goalies)). **I found no published accuracy figure for predicting starters before confirmation.**

### Tandem changes, trades and injuries

No source quantified what happens to a goalie's share when his partner is hurt or traded. The structural facts above imply the direction: the healthy goalie's share rises toward the workhorse ceiling (about 75% of games, the 2025-26 maximum) but not to 100%, because he still will not play both halves of back-to-backs. A goalie traded to a new team inherits that team's shot volume and win rate (both team properties) but keeps his own, weakly known, save talent.

## 4. Schedule effects

- **Games in the matchup week** are the first-order schedule effect in weekly head-to-head: a skater with four games has a third more opportunities than one with three. Daily Faceoff's schedule model treats four-game weeks as a secondary factor and notes teams with many four-game weeks also have more two-game weeks ([Daily Faceoff](https://www.dailyfaceoff.com/news/2026-27-fantasy-hockey-strength-of-schedule)).
- **Off-nights / light nights** are days when few teams play. Daily Faceoff currently defines one as a day with eight or fewer games (earlier: fewer than half the league playing) and reports that Mondays, Wednesdays, Fridays and Sundays average about five games ([Daily Faceoff](https://www.dailyfaceoff.com/news/2026-27-fantasy-hockey-strength-of-schedule), [Daily Faceoff 2024](https://www.dailyfaceoff.com/news/fantasy-hockey-2024-25-using-light-days-to-your-advantage-on-draft-day)). Their value is that a player on a light night is never benched for lack of a lineup slot. In 2024-25 the gap between the team with the most light-night games (37) and the fewest (17) was 20 games. The same article says to use it as a late-round tiebreaker, because good players are rarely benched anyway.
- **Back-to-backs** matter mainly through goalies (section 3). For 2026-27 Daily Faceoff builds a "weighted fatigue edge" from how often a team meets opponents on short rest and names Montreal, Calgary and Vegas as beneficiaries ([Daily Faceoff](https://www.dailyfaceoff.com/news/2026-27-fantasy-hockey-strength-of-schedule)); it does not publish the size of the effect on skater scoring.
- **Opponent strength.** For goalies the opponent's scoring and shot volume matter directly. For skaters, the opposing team's goals against is the relevant figure; the 2025-26 range was 2.40 to 3.83 goals against per game (own calculation). I found no verified estimate of travel effects.

## 5. Injuries and availability

- **Incidence by position.** Over 2007-08 to 2018-19, injury and illness incidence per 1,000 athlete-game exposures was 13.0 for centres, 10.9 for wings, 10.9 for defensemen and 5.1 for goalies; centres were about 1.4 times as likely to have a subsequent injury ([Martin et al., IJSPT 2024](https://ijspt.scholasticahq.com/article/92309-initial-versus-subsequent-injury-and-illness-and-temporal-trends-among-professional-hockey-players)). The paper's abstract and table disagree on the centre figure (15.1 against 13.0). The same paper cites earlier work finding that 50.9% of NHL players missed at least one game per season in 2009-12. It reports nothing by age. A separate study is reported to have found goalies more likely to have long absences (search-result summary only).
- **Games-played base rate (own calculation, a proxy).** Of skaters who were regulars in 2024-25 (60+ games and at least 14 minutes a game for forwards, 17 for defensemen), games played in 2025-26 averaged 72 of 82 for forwards aged 18-29, 67 for forwards 30-33, 70 for forwards 34+, about 69-72 for defensemen under 34, and 61 for defensemen 34+ (only 24 players). This is one season pair and counts scratches, demotions and retirements as well as injuries. A reasonable prior is that an established regular plays about 85-88% of games, lower for the oldest defensemen.
- **Return from injury.** A study of 94 concussed NHL players against 58 controls found no difference in performance or style of play in the five games after return ([Healio summary of Kuhn et al., Am J Sports Med 2016](https://www.healio.com/news/orthopedics/20161006/study-nhl-players-who-sustained-a-concussion-did-not-have-performance-style-of-play-changes)). Other studies (search-result summaries only) report mostly stable performance after hand and wrist fractures and mixed results after Achilles ruptures. The supported default is "no rate penalty on return", with attention to whether ice time is reduced at first.
- **Roster designations.** Day-to-day (DTD) means a short, undefined absence. IR is the NHL's injured reserve; LTIR is long-term injured reserve, used for long absences. On ESPN only players tagged IR or O (out) can go in an IR slot; a day-to-day player must sit on the bench ([ESPN support](https://support.espn.com/hc/en-us/articles/115003860512-Move-a-Player-On-or-Off-Injured-Reserve-IR)). I could not find Fantrax's rule; check the league settings.

## 6. Position eligibility

ESPN takes each player's eligibility from the NHL before the season, cannot change the primary position once the season starts, and may add positions during the season "if circumstances warrant it"; league managers cannot change positions ([ESPN support](https://support.espn.com/hc/en-us/articles/360054126392-Position-Eligibility)). I could not find Fantrax's official hockey rule. In leagues with separate C, LW and RW slots, multi-position players add lineup flexibility. **Both of our leagues use generic F slots, so that flexibility is worth nothing here.** What does matter is forward versus defense: the two pools have different replacement levels (section 7), so a player eligible at both, which is rare, should be valued at whichever slot gives more value over replacement.

## 7. How our scoring shifts value

### Exchange rates

| Statistic | ESPN league | As share of a goal | Fantrax league | As share of a goal |
|---|---|---|---|---|
| Goal | 2 | - | 4 | - |
| Assist | 1 | 50% | 2 | 50% |
| Power-play point | +0.5 | 25% | +1 | 25% |
| Shot on goal | 0.1 | 5% | 0.5 | 12.5% |
| Hit | 0.1 | 5% | 0.3 | 7.5% |
| Block | 0.5 | 25% | 0.5 | 12.5% |
| Penalty minute | 0 | 0 | 0.5 | 12.5% |

Our ESPN skater scoring matches ESPN's published default; our goalie scoring does not (default is win 4, goal against -2, shutout 3; ours is 2, -1.5, 2) ([ESPN](https://africa.espn.com/fantasy/hockey/story/_/id/34491407/which-fantasy-hockey-league-right-you)). Relative to ESPN, Fantrax pays 2.5 times as much for a shot, 1.5 times for a hit, half as much for a block, and adds penalty minutes.

### Where the points come from (own calculation, 2025-26, 40+ games)

| Group | Goals | Assists | PPP bonus | Shots | Hits | Blocks | PIM |
|---|---|---|---|---|---|---|---|
| ESPN, top 150 forwards | 37% | 25% | 6% | 13% | 5% | 14% | - |
| ESPN, top 80 defensemen | 14% | 22% | 4% | 10% | 5% | **45%** | - |
| Fantrax, top 110 forwards | 31% | 21% | 6% | **26%** | 6% | 5% | 5% |
| Fantrax, top 60 defensemen | 14% | 22% | 4% | **24%** | 8% | 19% | 8% |

Counting only goals, assists and special-teams points, 26 defensemen rank among the top 200 skaters per game; with full ESPN scoring 63 do, and with full Fantrax scoring 42.

Approximate replacement levels, taking the last starter as 9 forwards and 5 defensemen times 14 teams on ESPN and 6 and 4 times 12 on Fantrax, and ignoring the utility slot: ESPN forward 1.57 points per game, ESPN defenseman 1.46; Fantrax forward 4.55, Fantrax defenseman 3.33.

### Archetypes, with real 2025-26 lines (own calculation)

| Player type (example) | Line | ESPN pts/game | Fantrax pts/game |
|---|---|---|---|
| Shot-volume winger (Alex DeBrincat) | 41 G, 44 A, 287 SOG, 38 hits | 2.31 | 5.69 |
| Shot-volume, low assists (Cutter Gauthier) | 41 G, 28 A, 285 SOG | 2.16 | 5.67 |
| Playmaker, few shots (Ryan McLeod) | 14 G, 40 A, 90 SOG, 16 hits | about 1.2 | 2.89 |
| Hitting forward who scores (Kiefer Sherwood) | 23 G, 13 A, 172 SOG, 339 hits, 50 PIM | 1.83 | 5.02 |
| Pure hitter (Yakov Trenin) | 6 G, 17 A, 413 hits | 1.21 | 3.29 |
| Power forward with PIM (Tom Wilson) | 30 G, 32 A, 179 hits, 117 PIM | 2.20 | 5.88 |
| Shot-blocking defenseman (Jake McCabe) | 5 G, 20 A, 190 blocks, 104 hits | 1.79 | 3.21 |
| Offensive defenseman, few blocks (Erik Karlsson) | 15 G, 51 A, 26 PPP, 64 blocks | 1.94 | 4.36 |
| Does-everything defenseman (Moritz Seider) | 10 G, 50 A, 180 blocks, 128 hits, 187 SOG | 2.51 | 5.10 |
| PIM defenseman (Nikita Zadorov) | 22 points, 196 hits, 152 PIM | 1.30 | 3.55 |

Readings:

- **ESPN.** McCabe (25 points) is worth nearly as much as Karlsson (66 points): 143 against 146 total. A stay-at-home defenseman with 150+ blocks clears replacement level; the 15 biggest risers when hits, blocks and shots are added are all defensemen with 84 to 190 blocks. Hits barely register: Trenin's league-leading 413 hits are worth 41.3 points, the same as about 21 goals, and he still averages only 1.21 points per game, so pure hitters stay below replacement. Low-shot, low-block playmaking forwards fall the most.
- **Fantrax.** Shots carry forwards: Gauthier with 69 points matches DeBrincat with 85. Wilson (62 points) matches Martin Necas (100 points, 5.93). Hitters who also shoot (Sherwood) clear forward replacement; pure hitters and pure shot-blockers do not (McCabe at 3.21 is just under the defense line of 3.33). Playmakers with few shots lose the most.

### Goalies under our scoring (own calculation, 61 goalies with 20+ starts, 2025-26)

Each shot faced is worth `save value x save% - goal-against penalty x (1 - save%)`:

| System | Break-even save % | Value of 28 shots at .896 (league average) | At .910 | At .880 |
|---|---|---|---|---|
| ESPN, ours (0.2 / -1.5) | .882 | +0.65 | +1.31 | -0.11 |
| ESPN default (0.2 / -2) | .909 | -0.80 | +0.06 | -1.79 |
| Fantrax (0.25 / -1) | .800 | +3.36 | +3.85 | +2.80 |

| Measure | ESPN, ours | Fantrax |
|---|---|---|
| Mean points per game played | 1.73 | 5.88 |
| Range across goalies | 0.26 to 3.30 | 3.88 to 7.86 |
| Of which wins | 0.97 | 2.42 |
| Correlation of points/game with save % | 0.96 | 0.82 |
| Correlation of points/game with shots faced/game | 0.21 | 0.23 |
| Correlation of points/game with wins/game | 0.62 | 0.77 |
| Correlation of season total with starts | 0.74 | 0.92 |

- **ESPN:** value is almost entirely save percentage, which is the least predictable input. An average goalie start (1.73) is worth about one replacement-level skater game (1.57), and a bad goalie is worth nearly nothing: Kevin Lankinen (.875, 43 starts) totalled 12 points; Sergei Bobrovsky (.877, 27 wins) ranked 41st. The best season total (Andrei Vasilevskiy, 159) is only about 30 points above what a replacement-level forward produces in 82 games (about 129). Goalies are low-value, high-variance assets here, and a single bad start (5 goals on 25 shots in a loss is -3.5) can cost more than a good one gains.
- **Fantrax:** every start is positive (the worst regular averaged 3.88), an average start (5.88) beats a replacement forward game (4.55) and approaches a top-10 forward game (6.28), and season totals follow starts. Volume goalies on middling teams hold value: Juuse Saros (.894, 59 starts) ranked 11th on Fantrax against 17th on our ESPN scoring; Bobrovsky ranked 21st against 41st; Lukas Dostal (.888) 16th against 27th. Wins at 5 and shutouts at 7.5 still favour good teams: Karel Vejmelka (38 wins, .897) ranked 3rd.

## How this applies to our two leagues

**ESPN (14 teams, 9 F / 5 D / 1 UTIL / 2 G, daily lineups, 2 adds per matchup period of 7 days).** Five defense slots times 14 teams means 70 defensemen start, and at that depth blocks decide who is startable; target 150+ block defensemen with 20+ minutes rather than 30-point defensemen who do not block. Forwards are valued mostly on goals and assists, with shots and blocks as a floor. Goalies matter less than under default ESPN scoring because wins are worth half as much; prefer goalies with good underlying numbers on good defensive teams, avoid starting a weak goalie in a bad matchup (expected value is near zero with a large downside), and do not spend scarce moves streaming goalies. With only 2 adds per matchup period and daily lineups, light-night scheduling helps only at the margin of the five-man bench.

**Fantrax (12 teams, 6 F / 4 D / 2 G, keep 3 per team, daily lineups, 16-player roster cap, 3 moves per week).** Only 72 forwards and 48 defensemen start, so replacement level is high and stars matter more. Value shot volume first, then goals; hits and penalty minutes are a real bonus for players who also shoot (the Wilson or Sherwood type). Goalie starts are the most valuable per-game events available below the top tier of forwards: maximise the number of goalie starts in the two active slots (lineups change daily, so this is set night by night), and value a 55-start goalie on an average team above a 40-start goalie with a better save percentage. For the three keeper decisions per team (only the top 3 players carry over, so long-horizon aging matters for those slots and little elsewhere), published aging work puts the skater plateau at roughly ages 22 to 25 with gradual decline after ([Hockey Graphs](https://hockey-graphs.com/2017/03/23/a-new-look-at-aging-curves-for-nhl-skaters-part-1/)), while an older peer-reviewed estimate is later (27-28 for forwards; search-result summary only); physical statistics and their aging were not covered by any source I found.

## Implications for the engine

Well supported by sources or by the own calculations above:

1. **Project skaters as rate times opportunity.** Keep per-60-minute rates for shots, hits, blocks and penalty minutes (year-to-year correlation 0.76-0.93) separate from projected minutes. Test: a change in projected TOI changes projected peripherals proportionally.
2. **Project goals as shots times a regressed shooting percentage.** Shrink a player's current-season shooting percentage strongly toward his career rate and the positional average. Test: a player shooting 20% over 20 games is not projected above his career rate plus a small allowance.
3. **Project power-play points from PP share times team power-play output**, not from the player's recent power-play points. Use roughly 21-30 PPP per 82 games at 55%+ share and roughly 7 at 25-40% as sanity bounds. Test: moving a player from PP2 to PP1 roughly triples projected PPP.
4. **Detect role from actual recent ice time** (NHL per-game TOI and power-play TOI), and use line-combination pages only as an early signal. Test: when a line page and the last three games' ice time disagree, the engine flags it and trusts ice time.
5. **Regress goalie save percentage heavily.** With year-to-year correlation of 0.12-0.33, a goalie's projected save percentage should sit much closer to league average (.896 in 2025-26, and falling) than to his current-season figure. Test: no projected season save percentage outside about .885-.915 without multi-season evidence.
6. **Project goalie wins from the team**, using team strength and opponent, not the goalie's past win rate.
7. **Project shots faced from the team** (correlation 0.88 with team shots against).
8. **Never project one goalie for both games of a back-to-back** unless confirmed. Default: starter first night, backup second, and say that it is a default.
9. **Cap a clear starter's share at about 75% of team games**, and use about 62% as the default for a typical number-one.
10. **Value goalies per league with the break-even formula.** On ESPN the start/sit test for a goalie is expected points greater than the alternative of an empty slot (zero), which can fail for a below-average goalie against a strong offence. On Fantrax an available start should essentially always be used.
11. **Value over replacement must be computed per position per league** (forward and defense lines are 1.57/1.46 on ESPN and 4.55/3.33 on Fantrax by my estimate). Ignore C/LW/RW.
12. **Treat ESPN IR eligibility as "tagged IR or O"**; a day-to-day player occupies a bench slot.
13. **Use confirmed starters when available**; Daily Faceoff labels entries Confirmed or Unconfirmed with a timestamp.

My own inference, to be validated:

14. **After a partner's injury or trade,** set the remaining goalie's share to min(75%, old share plus most of the partner's share), minus one start per team back-to-back in the period. **After a goalie changes teams,** reset shots faced and win probability to the new team's values, keep his regressed save percentage, and start his share at the departed goalie's share. **After a goalie returns from injury,** start at a 50% share for the first week unless confirmations say otherwise, and return to the prior share after two confirmed consecutive starts.
15. **Weight recent games for deployment and long history for rates:** last 5-10 games for TOI and PP share; one to three seasons for per-minute rates.
16. **Availability prior:** an established regular plays about 85-88% of remaining games; reduce for defensemen 34 and older. No performance penalty on return from injury, but use the first game's actual ice time before restoring the full projection.
17. **Rink adjustment for hits and blocks** is probably worthwhile (home/road splits per team) but the published rink effects are from 2007-13; measure current ones before applying any.
18. **Schedule value is the count of usable games,** meaning games on days when the player would actually be in the lineup, so it is zero for stars and positive only for players near the bench line.
19. **Flag unsustainable lines:** secondary-assist share above about 33% (forwards) or 50% (defensemen), IPP well above the player's norm, and on-ice shooting percentage above about 11% are signals to project regression. The thresholds come from DobberHockey and JFresh; applying them as automatic sell flags is my inference.

## Open questions / where sources disagree

- **Back-to-back goalie penalty:** Tulsky's figure is reported as "just over 11 percent" by NHL.com and as about one percentage point elsewhere; later work found less. Hockey Graphs found 8 points of save percentage but flagged confounding. The size is unsettled; the usage pattern is not.
- **Blocks and hits:** worth points in our leagues, worth nothing or less in real-hockey win models (Daily Faceoff). This matters only if the engine uses "real" player-quality models to predict ice time.
- **Zone starts:** DobberHockey treats them as important; other analysts find the direct effect small.
- **Secondary assists:** conventional wisdom says luck; my three-season data shows them only modestly less repeatable than primary assists at season scale.
- **Peak age:** about 22-25 (Hockey Graphs, value-based) against 27-28 (older scoring-based study).
- **ESPN default goalie scoring** in ESPN's article (win 4, goal against -2) differs from what a third-party site and the league owner report; the league's own settings page is the authority.
- **Not established by any source I found:** accuracy of projected line combinations; accuracy of pre-confirmation goalie-start predictions; how many games a role change takes to show in production; games-missed base rates by age; travel effects; Fantrax's official position-eligibility and IR rules; what "DEF 0.1" (ESPN) and "FT 3" (Fantrax) measure (since resolved: fights for FT, and most likely defensemen points for DEF; see the correction near the top). (The Fantrax lineup lock question is resolved: lineups change daily, per the league's Rules page and the official API.)

## Sources

Read in full (or queried directly):

- https://api.nhle.com/stats/rest/en/skater/summary?limit=-1&cayenneExp=seasonId=20252026%20and%20gameTypeId=2 (and the realtime, timeonice, scoringpergame, faceoffwins, powerplay and bios reports; seasons 20232024, 20242025, 20252026)
- https://api.nhle.com/stats/rest/en/goalie/summary?limit=-1&cayenneExp=seasonId=20252026%20and%20gameTypeId=2 (and the daysrest report; same three seasons)
- https://api.nhle.com/stats/rest/en/team/summary?limit=-1&cayenneExp=seasonId=20252026%20and%20gameTypeId=2 (and the powerplaytime report)
- https://hockey-graphs.com/2014/12/01/goaltender-performance-vs-rest/
- https://hockey-graphs.com/2014/01/20/2013-2014-goaltending-performance-save-percentage-correlation/
- https://hockey-graphs.com/2015/10/08/why-linemate-and-competition-metrics-may-not-be-as-simple-as-we-think/
- https://hockey-graphs.com/2016/01/27/redefining-shot-quality-one-pass-at-a-time/
- https://hockey-graphs.com/2017/03/23/a-new-look-at-aging-curves-for-nhl-skaters-part-1/
- https://nhl.com/kraken/news/core-concepts-examining-goalie-workload-329507592
- https://jfresh.substack.com/p/percentage-luck-in-hockey-explained
- https://moneypuck.com/glossary.htm
- https://ar5iv.arxiv.org/html/1412.1035
- https://ijspt.scholasticahq.com/article/92309-initial-versus-subsequent-injury-and-illness-and-temporal-trends-among-professional-hockey-players
- https://www.healio.com/news/orthopedics/20161006/study-nhl-players-who-sustained-a-concussion-did-not-have-performance-style-of-play-changes
- https://dobberhockey.com/2024/11/17/geek-of-the-week-peripheral-category-values/
- https://dobberhockey.com/2024/07/23/ramblings-changes-in-shots-hits-blocks-and-pims-in-recent-seasons-july-23
- https://dobberhockey.com/2021/02/10/goldipucks-and-the-three-skaters-crunching-the-numbers
- https://dobberhockey.com/2022/11/21/wild-west-why-zone-starts-are-more-important-than-you-think
- https://dobberhockey.com/2024/11/04/the-wild-west-power-play-changes
- https://www.dailyfaceoff.com/news/using-analytics-to-maximize-your-fantasy-hockey-leagues-scoring-system
- https://www.dailyfaceoff.com/news/2026-27-fantasy-hockey-strength-of-schedule
- https://www.dailyfaceoff.com/news/fantasy-hockey-2024-25-using-light-days-to-your-advantage-on-draft-day
- https://www.dailyfaceoff.com/news/fantasy-hockey-2025-26-weekly-strength-of-schedule-and-streaming-targets-week-20
- https://dailyfaceoff.com/news/backup-goalies-nhl-more-important-reliable-quality-starts-charlie-lindgren-casey-desmith
- https://www.dailyfaceoff.com/starting-goalies
- https://www.dailyfaceoff.com/team-line-combinations/ (seen in search results; describes posting practice only)
- https://africa.espn.com/fantasy/hockey/story/_/id/34491407/which-fantasy-hockey-league-right-you
- https://africa.espn.com/fhl/story/_/id/34823209/espn-fantasy-hockey-let-talk-hits-blocks-which-players-get-both
- https://support.espn.com/hc/en-us/articles/360054126392-Position-Eligibility
- https://support.espn.com/hc/en-us/articles/115003860512-Move-a-Player-On-or-Off-Injured-Reserve-IR
- https://thehockeynews.com/nhl/ottawa-senators/latest-news/ottawa-senators-buck-the-trend-why-dont-goalies-play-on-consecutive-days-anymore
- https://www.hockey-reference.com/about/glossary.html (checked; does not define quality starts)

Seen only as search-result summaries (claims from these are marked as such above):

- https://www.sports-reference.com/blog/2014/12/games-started-quality-starts-on-hockey-reference-com/
- https://africa.espn.com/fhl/story/_/id/35342075/espn-nhl-power-play-efficiency-ottawa-senators-alex-iaffalo-tyler-johnson
- https://thehockeynews.com/news/news/updated-2022-23-nhl-fantasy-hockey-rankings-top-10-defensemen
- https://www.thescore.com/nhl/news/1264245
- https://ideas.repec.org/a/bpj/jqsprt/v10y2014i2p19n6.html
- https://thehockeywriters.com/jeff-blashill-vs-advanced-stats/
- https://canucksarmy.com/news/small-but-not-insignificant
