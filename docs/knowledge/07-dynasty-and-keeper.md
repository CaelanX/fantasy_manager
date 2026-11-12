# 07 - Dynasty and keeper league strategy

Researched 2026-11-10. Every factual claim links to the page it was read from. Where a number was seen only in a search-result snippet and not on the page itself, it is marked "(snippet only)". Sections labelled "inference" are reasoning by the author of this file, not published findings.

## What this covers

This file covers how to value players across several seasons in a league where rosters carry over: how dynasty and keeper formats differ from redraft, how much a future season is worth next to the current one, how hockey players age by position, how often drafted prospects become useful and how long they take, what draft picks are worth, how to decide between contending and rebuilding, how to choose keepers, and how the trade market misprices youth, veterans and goalies. It ends with rules for our 12-team Fantrax head-to-head points league (labelled Dynasty by Fantrax, but each team keeps only its top 3 players; see the scope note in "How this applies") and for the engine's dynasty value and contend/rebuild recommendation.

## Key takeaways

- Skater scoring peaks in the mid-20s and holds for years. Published peak estimates range from about 24-25 (delta-method and wins-above-replacement studies) to 27-28 for forwards and 28-29 for defensemen (a regression study on points per game). The disagreement is about method, and the practical message is a flat plateau from roughly 23 to 29 with real decline in the 30s.
- Goalies do not show a clear development peak in the research. One peer-reviewed study found goalie performance "varies little by age"; a save-percentage study found decline visible by 30 and rapid by the mid-30s. Fantasy usefulness arrives late: goalies debut at about 24 on average and Dobber's rule of thumb is that they help a fantasy team at about 25.
- Draft slot matters enormously and falls off fast. In one 10-draft sample 74% of first-round picks reached 100 NHL games against 34% of second-round picks. Market-based pick charts put the 30th pick at about 14% of the 1st pick and the 60th at under 4%.
- Prospects are slow. Top-3 picks take 1-2 years to become full-time NHL players, picks 16-30 take 3.5-4 years, and Dobber's "breakout threshold" says a forward needs about 200 NHL games (400 for defensemen and very big or small forwards) before producing at his real level.
- Prospect value typically dips between draft day and that breakout. This is the argument for trading dynasty draft picks and freshly drafted prospects at peak hype, and for buying post-hype players at 150-300 NHL games.
- Dynasty markets pay a premium for youth. In a Fantrax dynasty draft, players over 30 went far later than their redraft position, which makes productive veterans the cheapest source of current points for a contender.
- The best team usually does not win a head-to-head title. One model has the better team winning 55-60% of playoff matchups, which caps even the top team's title odds at roughly 30-36% with a bye. This argues against paying a large value premium for a small gain in this year's odds.
- Rebuilding means accepting title odds of zero this year, so it only pays if later seasons carry clearly above-average odds. In a 12-team league a team that punts one season needs about 16.7% title odds the next season (double the baseline 8.3%) just to break even.
- Financial-style prospect valuation in baseball uses an 8% annual discount rate. Fantasy leagues justify a higher rate because of league-folding risk, projection error and the randomness of titles; the common hockey heuristic is to value a player on his next three years.
- With no minors slots, every prospect who is not in the NHL costs a full reserve slot that could hold a producing player. Published advice is that a two-year wait "is a long wait in fantasy" and is only worth it in deep formats.
- Goalie outcomes are noisy. Save percentage needs on the order of 1,500-3,000 shots to stabilise, so one season says little, and goalie draft position is weakly related to career games.
- No rigorous public evidence was found on how accurate the named prospect rankers (Pronman, Wheeler, Elite Prospects, DobberProspects) have been. Treat their ranks as inputs, not as calibrated probabilities.

## 1. Formats: redraft, keeper, dynasty and their variants

A redraft league starts from an empty roster every year, so only this season's production matters. A keeper league lets each team retain a limited number of players. A dynasty league retains the whole roster, and the yearly draft becomes a rookie or prospect draft. Yahoo's guide draws the line this way: "Keeper leagues differ from dynasty leagues as you're only keeping some players, not the entire team" ([Yahoo Sports](https://ca.sports.yahoo.com/news/fantasy-hockey-keeper-league-rules-rankings-strategy-advice-140748389.html)).

The main variants and their strategic effect:

- Limited keepers. The fewer the keepers, the faster weak teams can recover: "The fewer keepers you have, the easier it will be for bad teams to improve themselves in a relatively short time period." In a league that kept about 70% of rosters, the top 5-6 teams stayed "virtually the same for five seasons" ([DobberHockey](https://dobberhockey.com/?p=26057)).
- Keeper cost or round escalation. Some leagues charge a draft pick tied to the round the player was drafted in, which makes late-round finds the most valuable keepers; others charge nothing or cap the number of years a player can be kept ([Yahoo Sports](https://ca.sports.yahoo.com/news/fantasy-hockey-keeper-league-rules-rankings-strategy-advice-140748389.html)). Tenure caps (for example five years) and limits tied to finishing position are also used to recycle stars ([DobberHockey](https://dobberhockey.com/?p=78760)).
- Contracts and salary caps. Contract-year budgets and cap space become tradeable assets; one rebuilding guide calls cap-space trades "the most powerful tool" for a rebuilding team ([DobberHockey](https://dobberhockey.com/2013/07/18/rebuilding-to-contention-part-iii-tips-along-the-way)).
- Minors or farm slots. A farm is a separate set of roster spots for prospects that do not count against the active roster. One 14-team example carried 22 NHL players plus 9 farm prospects ([DobberHockey](https://dobberhockey.com/?p=26057)). On Fantrax, prospects can be drafted in a separate draft or mixed with NHL players, and a typical prospect definition is under 24 with 150 or fewer NHL games ([Fantrax HQ](https://fantraxhq.com/?p=149855)).

Our league is a full dynasty with tradeable picks and no farm, which is the variant that punishes prospect hoarding the most (see section 8).

## 2. Valuing players over several seasons

Time discounting means counting a future season at less than a current one. Three separate reasons justify it: a pure preference for winning sooner, the growing error in projections further out, and the chance the league, the player's role or your own team changes before the value arrives.

The published anchors found:

- Baseball surplus-value work discounts future production at 8% a year ([FanGraphs, Craig Edwards 2018](https://blogs.fangraphs.com/an-update-to-prospect-valuation/)). A later FanGraphs update was reported as using a net 7% (10% less 3% wage inflation), but that page was not read directly (snippet only).
- Hockey keeper advice commonly uses a "three year rule", valuing a player on his next three years of production. A rebuilding team is told to abandon it: "Don't put any value on the next year or two of production" ([DobberHockey](https://dobberhockey.com/2013/07/18/rebuilding-to-contention-part-iii-tips-along-the-way)).
- Keeper (not dynasty) advice is shorter still: focus on this season and the next ([Yahoo Sports](https://ca.sports.yahoo.com/news/fantasy-hockey-keeper-league-rules-rankings-strategy-advice-140748389.html)).

The "window" is the span of seasons in which a roster is good enough to contend. Sanderson's framework describes a window team as carrying a lot of "productive value" while competing and converting it into picks and young players when rebuilding, and calls both the all-in and the tanking approach "credit constructions" that borrow from one period to pay another ([Thinking About Thinking](https://jakobsanderson.substack.com/p/market-principles-of-dynasty-part-2d7)).

Why managers overpay for youth and picks. Draft picks "represent Pure Potential" and prospects usually lose value between draft day and their breakout ([DobberHockey](https://dobberhockey.com/2024/06/01/the-journey-top-36-draft-ranking-zero-picks-strategy-prospects-who-slipped-through-a-5-round-12-team-dynasty-draft)). On Fantrax, "players over 30 seem to become toxic to own": in one dynasty draft Ovechkin went 174th against a 65.79 redraft average draft position and Hedman 108th against 83.69 ([Fantrax HQ](https://fantraxhq.com/?p=149855)).

Evidence on win-now versus build-young. No study was found that counts championships by strategy in hockey, and the football equivalent searched for was not found either. What exists is modelling and reasoning:

- Harstad estimates "the better team wins about 55-60% of matchups in the playoffs", which gives the best team roughly a 30-36% title chance with a bye and 16-20% without ([Footballguys](https://www.footballguys.com/article/2023-dynasty-in-theory-68-when-to-rebuild)). His conclusion is that managers overestimate the top team's odds and underestimate a middling playoff team's.
- In his 12-team model with every game a coin flip, a guaranteed regular-season win raises title odds from 8.3% to about 10.3%, while a guaranteed top score in the final two weeks raises them to 16.6% ([Footballguys](https://footballguys.com/article/2024-dynasty-in-theory-lets-talk-about-win-now-trades)). Playoff-week strength is worth far more than regular-season strength.
- He defines a win-now trade as one where you receive less total value than you give in order to raise near-term odds, and argues against them: "Chase value, not wins" ([Footballguys](https://www.footballguys.com/article/2023-dynasty-in-theory-65-chase-value-not-wins)).
- Sanderson's arithmetic: with 8.33% baseline odds per year, a team that takes 0% in year one needs about 16.67% in year two, or 12.5% in each of years two and three, to get back to even ([Thinking About Thinking](https://jakobsanderson.substack.com/p/market-principles-of-dynasty-part-2d7)).
- The Fantrax HQ writer's preferred hockey startup build is "prime players, win now", targeting ages 24 to 30, and he warns that a youth-only build means "you won't make the playoffs in the first year" with contention around year three ([Fantrax HQ](https://fantraxhq.com/?p=149855)).

## 3. Aging curves in hockey

An aging curve is the average change in performance with age. The "delta method" builds it by averaging year-over-year changes for players who played at both ages. Survivorship bias is the distortion that arises because only good players are still employed at older ages, which makes raw averages look too flat late in careers.

| Study | Metric and method | Forwards | Defensemen | Goalies |
|---|---|---|---|---|
| [Brander, Egan and Yeung 2014, via UBC release](https://www.eurekalert.org/news-releases/880909) | Points per game and plus-minus, 1997-98 to 2011-12, 2,033 players | Peak 27-28; near peak 24-32 | Peak 28-29; near peak 24-34 | Performance "varies little by age" |
| [EvolvingWild 2017, part 1](https://hockey-graphs.com/2017/03/23/a-new-look-at-aging-curves-for-nhl-skaters-part-1/) | Wins above replacement, delta method, uncorrected | Average skater "plateaus from age 22 to 25" | "plateau from ages 19 to 24 and decline steadily after that" (small young sample) | Not covered |
| [EvolvingWild 2017, part 2](https://hockey-graphs.com/2017/04/10/a-new-look-at-aging-curves-for-nhl-skaters-part-2/) | Same, with survivorship correction | "peak around 24-25 years old and decline gradually"; decline after 30 "might not be as steep as is often assumed" | Same statement for skaters overall | Not covered |
| [Schuckers, Lopez and Macdonald 2021](https://ar5iv.arxiv.org/html/2110.14017) | Standardised points per game, forwards only | Delta-method curves "tending to peak around age 26" | Not analysed | Not covered |
| [Sabermetric Musings 2020](https://sabermetricmusings.blogspot.com/2020/07/nhl-skill-aging-curves-scoring-and-shot.html) | Even-strength rates, delta method | Scoring rate peaks "around 24"; shot rate peaks "around age 22"; quick decline in the 30s | Not covered | Not covered |
| [garik16 2014](https://hockey-graphs.com/2014/03/21/how-well-do-goalies-age-a-look-at-a-goalie-aging-curve/) | Save percentage relative to average, 800+ goalie seasons | - | - | Worse every year on average; decline "REALLY noticable" by 30; about -.004 to -.009 per year from 35 to 40 |

Points to draw from the table:

- Peak age depends on the method. Rate and value metrics peak earlier (24-26) than raw points per game (27-29). A plausible reconciliation, offered here as inference, is that ice time and power-play role keep growing after per-minute skill has peaked, so fantasy points (which reward volume) peak later than per-minute value.
- The Schuckers paper explains why older-age estimates run high: "the only players competing in their late 30s tend to be very good players." It reports that about 63% of forwards are observed at ages 23-24 but fewer than 1 in 10 at 36 ([arXiv 2110.14017](https://ar5iv.arxiv.org/html/2110.14017)). For fantasy this means the risk for an older player is less a slow slide and more a growing chance of dropping out of a scoring role entirely.
- Shot rate peaking at 22 matters for our scoring, which pays 0.5 per shot.
- Defensemen: the points-based study puts their peak a year later than forwards and their near-peak band two years longer (to 34). The value-based study does not support a late peak. Fantasy development is slower: Dobber's breakout threshold is about 400 NHL games for most defensemen against 200 for average-sized forwards ([DobberHockey](https://dobberhockey.com/2022/06/30/breakout-threshold-bt-using-career-games-played-to-determine-breakout-potential)).
- Goalies: average NHL debut age over a ten-year span was 24.2 for goalies, 21.8 for forwards and 22.1 for defensemen ([NHL.com / Kraken](https://www.nhl.com/kraken/news/power-of-patience-nhl-draft-compared-to-nba-nfl-334766228)). Dobber's rule: "goalies need to be 25" before they significantly help a fantasy team ([DobberHockey](https://dobberhockey.com/?p=67718)). For a .920 goalie at 35 the goalie study projects .909 to .912 two years later ([Hockey Graphs](https://hockey-graphs.com/2014/03/21/how-well-do-goalies-age-a-look-at-a-goalie-aging-curve/)).

No published aging curves were found for hits, blocks or penalty minutes.

## 4. Prospects

### How often draft picks make it

| Measure | Round 1 | Round 2 | Round 3 | Round 4 | Round 5 | Round 6 | Round 7 | Source |
|---|---|---|---|---|---|---|---|---|
| Played at least one NHL game, 1988-1997 drafts | 91.7% | 65.0% | 56.7% | 38.0% | 32.3% | 27.3% | 21.0% | [Schuckers 2011](https://myslu.stlawu.edu/~msch/sports/Schuckers_NHL_Draft.pdf) |
| Played at least one NHL game, ten most recent drafts as of 2022 | 84.3% | 51.1% | 37.6% | under 30% | under 30% | under 30% | 13.3% | [NHL.com / Kraken](https://www.nhl.com/kraken/news/power-of-patience-nhl-draft-compared-to-nba-nfl-334766228) |
| Reached 100 NHL games, 2000-2009 drafts | 74% | 34% | not stated | | | | | [DobberProspects](https://dobberprospects.com/2020/05/16/nhl-draft-pick-probabilities/) |

Other figures from the same sources: the fitted chance of playing 200 or more games is about 98% for the 1st pick and 8% for the 210th, falling steeply to about pick 45 and gently after; overall 25% of drafted forwards, 21% of defensemen and 19% of goalies passed 200 games ([Schuckers 2011](https://myslu.stlawu.edu/~msch/sports/Schuckers_NHL_Draft.pdf)). An average draft class produces about 60 players who reach 100 games and about 40 who reach 300; picks 1-5 all reached 100 games in 2000-2009, while only 4 of 10 players taken 15th did and none taken 48th ([DobberProspects](https://dobberprospects.com/2020/05/16/nhl-draft-pick-probabilities/)).

Reaching the NHL is not fantasy relevance. Byron Bader's forward study defines an "impact point producer" as 0.6 career points per game or better over 100+ games:

| Forward profile | Reached NHL (100+ games) | At least 0.4 points per game | 0.6+ points per game |
|---|---|---|---|
| First-round pick, draft-year NHLe of 30+, drafted before 19 | 91% | 85% | 59% |
| Same, NHLe of 34+ | 100% | 100% | 70% |
| NHLe of 30+ before age 20, any round | 91% | 66% | 37% |
| First reached NHLe 30+ after age 20 | 70% | 29% | not stated |
| Never reached NHLe 30+ | not stated | 18% | not stated |

Source: [Flames Nation, Bader 2015](https://flamesnation.ca/news/predicting-future-nhl-scoring-success-with-nhle-thresholds-related-factors), forwards drafted 2004-2014.

### NHL equivalency

NHL equivalency (NHLe) converts scoring in another league into an expected NHL scoring rate by multiplying by a league factor. Patrick Bacon's network model expresses the factor as the NHL value of one point in that league:

| League | Factor | League | Factor |
|---|---|---|---|
| KHL | 0.772 | NCAA | 0.194 |
| SHL | 0.566 | OHL | 0.144 |
| Liiga | 0.441 | USHL | 0.143 |
| AHL | 0.389 | WHL | 0.141 |
| Allsvenskan | 0.351 | QMJHL | 0.113 |

Source: [hockeystats.com](https://hockeystats.com/methodology/nhle). Older factor sets differ noticeably (for example an AHL factor of 0.44 and an NCAA factor of 0.41 appeared in search results, snippet only), so factors must be taken from one consistent, current source. The same page defines an "NHLer" as 200+ games with positive career value, a "star" as roughly the top 18.5% by value per game, and a bust as anyone not an NHLer by the eighth season after the draft. It notes that "the hardest thing for scoring rates as a prospect to predict is whether a defenseman will become an NHL star", and it does not cover goalies.

### How long prospects take

| Draft slot | Average years to full-time NHL (41+ games) | Share reaching full-time |
|---|---|---|
| 1 | 1.0 | 100% |
| 2-3 | 1.6-1.8 | 100% |
| 4-10 | 1.7-3.1 | 80-100% |
| 11-15 | 2.9 | 78% |
| 16-20 | 4.0 | 76% |
| 21-25 | 3.6 | 80% (expected to fall) |
| 26-30 | 3.7 | 70% (expected to fall) |

Source: [DobberProspects, Soboleski 2016](https://dobberprospects.com/?p=9103), skaters only, ten drafts before 2015; the averages count only players who made it.

Arrival is not production. The breakout threshold says a player shows his real level at about 200 career NHL games if he is an average-sized forward (5-10 to 6-2, 171 to 214 pounds) and about 400 games for bigger defensemen and exceptionally sized forwards, with a breakout defined as a 25% year-over-year jump and the rule holding for roughly 80% of players ([DobberHockey](https://dobberhockey.com/2022/06/30/breakout-threshold-bt-using-career-games-played-to-determine-breakout-potential)). Gehrels summarises the total wait: "there is almost always a 3-4+ year wait between when a player is drafted and when they become fantasy relevant" ([DobberHockey](https://dobberhockey.com/2024/06/01/the-journey-top-36-draft-ranking-zero-picks-strategy-prospects-who-slipped-through-a-5-round-12-team-dynasty-draft)).

### How the experts rank, and how reliable they are

Hockey Prospecting (Bader) gives each skater a probability of becoming an NHLer and a star from NHLe history; it is points-only and has no goalie model (search snippets of [Flames Nation](https://flamesnation.ca/news/an-argument-for-nhle), not read in full). DobberProspects publishes fantasy-specific ranks and a peak-points projection called PNHLe, and Dobber's organisational prospect ratings run on a 10-point scale ([DobberHockey](https://dobberhockey.com/2024/06/01/the-journey-top-36-draft-ranking-zero-picks-strategy-prospects-who-slipped-through-a-5-round-12-team-dynasty-draft)). Elite Prospects' ranking is reported to start from projected peak role and then weigh proximity (snippet only; the page returned an error). Scott Wheeler's and Corey Pronman's rankings at The Athletic are scouting-based and could not be read.

No study was found that scores any of these public rankers against outcomes. The nearest evidence is that NHL teams' own draft order beat Central Scouting's rankings, worth an estimated $1.8M to $5.2M per team per year, with no single team clearly better than the rest ([Schuckers and Argeris](https://arxiv.org/abs/1411.5754)). The defensible reading is that actual draft slot is the best-validated single predictor, production (NHLe) adds information, and expert ranks are useful mainly for players whose value has moved since the draft.

### Goalie prospects

- About 68% of drafted goalies are 18, and an 18-year-old is "most likely 8 to 10 years away from their peak"; draft position and games played correlate at only r = -0.24 ([Canucks Army](https://canucksarmy.com/news/a-recent-history-of-drafted-goaltenders)).
- In the 1988-1997 drafts, goalie career games drop sharply after round 2 and are then roughly level through rounds 3 to 7, unlike skaters ([Schuckers 2011](https://myslu.stlawu.edu/~msch/sports/Schuckers_NHL_Draft.pdf)).
- Of 62 NHL goalies in October 2018, 13 were undrafted and 6 of those were starters ([The Hockey News](https://thehockeynews.com/news/from-the-draft-to-the-nhl-a-round-by-round-look-at-the-leagues-skaters-and-goalies)).
- Fantasy advice follows: "it'll probably take 3-4 years for the investment to pay off" ([RotoWire](https://www.rotowire.com/hockey/article/dynasty-league-strategy-58771)); Dobber drafts goalies aged 21-23 rather than 18 ([DobberHockey](https://dobberhockey.com/?p=67718)); Gehrels prefers "older tenders on the verge of NHL action" and recommended selling a hyped college goalie who was four or five years away ([DobberHockey](https://dobberhockey.com/2022/06/04/the-journey-potential-stars-with-longer-timelines)).

## 5. Draft pick value

Two published NHL pick charts bracket the answer. Schuckers values a pick by expected career games; Tulsky values it by what teams actually paid in 46 pick-only trades from 2006 to 2012.

| Pick | Schuckers expected games (share of 1st) | Tulsky market value (1st = 100) |
|---|---|---|
| 1 | 917 (100%) | 100 |
| 2 | 871 (95%) | 69.9 |
| 5 | 741 (81%) | 49.2 |
| 10 | 565 (62%) | 36.5 |
| 20 | 350 (38%) | 22.4 |
| 30 | 265 (29%) | 14.2 |
| 45 | 192 (21%) | 7.2 |
| 60 | 175 (19%) | 3.8 |
| 90 | 131 (14%) | 1.1 |
| 120 | 88 (10%) | 0.40 |
| 210 | 51 (6%) | 0.10 |

Sources: [Schuckers 2011](https://myslu.stlawu.edu/~msch/sports/Schuckers_NHL_Draft.pdf), [Tulsky 2013](https://www.broadstreethockey.com/post/nhl-draft-pick-value-trading-up/). Percentages in the Schuckers column are computed here from his values.

The games-based chart is far flatter because a 500-game checking forward counts the same as a 500-game scorer, which Schuckers himself notes. Fantasy value comes almost entirely from scorers, so the steep market chart is the better guide for a dynasty rookie draft (inference). A later trade-based study found that top picks cost more in trade than the players selected returned, that the two converge around pick 35, and that the market undervalues second- and third-round picks, with pick 64 the most undervalued ([Sound of Hockey](https://soundofhockey.com/2022/06/06/examining-the-value-of-nhl-draft-picks/)). The same author found NHL teams charge only a small premium to move a mid-round pick one year into the future, roughly a late fourth-round pick for third- and fourth-rounders ([Sound of Hockey](https://soundofhockey.com/2022/06/10/understanding-the-market-for-future-nhl-draft-picks/)).

Dynasty football hit rates show the same cliff: 83% for the first pick, 71% for picks 2-4, 38-46% for the rest of round one, about 20% in round two and 8% in round three, and "the value of draft picks will only increase the closer the time comes to your rookie drafts" ([Dynasty Nerds](https://www.dynastynerds.com/dynasty/dynasty-trade-secrets-understanding-draft-pick-values/)).

How deep useful talent goes in hockey: after a 5-round, 12-team dynasty prospect draft (60 picks), twenty useful prospects were still unclaimed, and all twenty were rostered within the year ([DobberHockey](https://dobberhockey.com/2024/06/01/the-journey-top-36-draft-ranking-zero-picks-strategy-prospects-who-slipped-through-a-5-round-12-team-dynasty-draft)). That league has farm rosters; in a league without them the useful depth is much shallower because few teams can afford to hold a late pick.

## 6. Contend or rebuild

Deciding. The clearest indicator is projected finish: "Ideally, projections have you as a top-four team" to contend, while a terrible projected win total, bottom-third starters and players past their peak point to a rebuild ([Footballguys](https://www.footballguys.com/article/2025-rebuilding-or-contending-the-dynasty-lifecycle)). The hockey community rule is "if you can't win in the next three years without unusual movement and luck, it's time to rebuild" ([DobberHockey](https://dobberhockey.com/?p=26758)). Harstad's counterweight is that a rebuild means reducing this year's odds "all the way to zero" and that "your title odds are never zero until your title odds are zero", so a team that can make the playoffs should usually keep its chance ([Footballguys](https://www.footballguys.com/article/2023-dynasty-in-theory-68-when-to-rebuild)).

Executing a contend. Buy productive veterans from teams that want picks, strengthen the weakest lineup position, add injury cover, weight the playoff weeks, and "don't over-mortgage the future" ([Footballguys](https://www.footballguys.com/article/2025-rebuilding-or-contending-the-dynasty-lifecycle)). Keep some prospects: "you can't load your full team with prospects if you plan on going for it, but you also can't go without any" (DobberHockey rebuilding series, snippet only).

Executing a rebuild. Commit fully rather than making "a half-hearted attempt"; sell players who will age out before you are competitive; trade with contenders, who value near-term production more; buy low on injured, buried or underperforming young players; keep open roster spots to cycle breakout candidates and resell them; and still start your best lineup rather than tanking ([DobberHockey part I](https://dobberhockey.com/?p=26758), [part III](https://dobberhockey.com/2013/07/18/rebuilding-to-contention-part-iii-tips-along-the-way)). The planned timeline in that series is last place, then near the playoffs, then contention in year three.

The cost of the middle. No source read quantifies it directly. The arithmetic in section 2 implies it: a team that neither raises this year's odds nor acquires future value keeps roughly baseline odds, while the teams around it trade toward one side or the other (inference).

## 7. Keeper selection

Keeper surplus is the value of a kept player above what the same slot or pick cost would return in the draft. The published guidance: late-round picks are "vital" in cost-based leagues because they allow keeping a player for far less than he is worth; compare rosters across the league to see which good players other teams cannot protect; buy those players without overpaying because "you're the one who holds all the leverage"; and if only a few players can be kept, trade a marginal keeper for a strong offer rather than lose him for nothing ([Yahoo Sports](https://ca.sports.yahoo.com/news/fantasy-hockey-keeper-league-rules-rankings-strategy-advice-140748389.html)). In a full dynasty the same idea applies at the roster margin: the weakest players on a full roster are worth only what the best free agent is worth, so they should be packaged two-for-one before a roster cut-down rather than dropped (inference). In our Fantrax league only 3 players per team are kept and the rest are redrafted, so the cut-down is from 16 players to 3 and the "weakest players" simply return to the draft; the keeper-surplus logic above applies to the three keeper slots, and the full-dynasty margin logic does not.

## 8. Small rosters and the cost of holding prospects

Published advice treats bench space as the binding limit. A defense prospect at least two years from the NHL is "a long wait in fantasy" and "in most formats, that is too long to stash a prospect on your bench" ([DobberHockey](https://dobberhockey.com/2022/06/04/the-journey-potential-stars-with-longer-timelines)). A farm system is what "lets a team use more of a risk/reward strategy on young players" (DobberHockey, snippet only). Rebuilders are told to keep a couple of roster spots open for churning short-term players ([DobberHockey](https://dobberhockey.com/2013/07/18/rebuilding-to-contention-part-iii-tips-along-the-way)). No source gives a number for how many non-producing assets a contender can carry; section "How this applies" gives an inferred figure.

## 9. Trade market behaviour

- Youth premium. Players over 30 are discounted well below their production (section 2), and a contender can exploit this ([Fantrax HQ](https://fantraxhq.com/?p=149855)).
- Hype cycle. Value is highest at the draft, falls during the wait, and recovers at the breakout. "Fantasy managers have short memories when it comes to young players and prospects"; Jack Hughes was cheapest after seasons of 28 and 45 points and before a 94-point season, and MacKinnon needed about 300 games ([DobberHockey](https://dobberhockey.com/?p=73269)). Lafreniere took "over 300 NHL games and the better part of four years" to recover his value ([DobberHockey](https://dobberhockey.com/2024/06/01/the-journey-top-36-draft-ranking-zero-picks-strategy-prospects-who-slipped-through-a-5-round-12-team-dynasty-draft)).
- Post-hype sleepers. The buy window is a high-pedigree player approaching his breakout threshold whose results so far are ordinary, especially a big forward on the 400-game track ([DobberHockey](https://dobberhockey.com/?p=73269)).
- Selling veterans. Rebuilders should sell "productive players who will age out before you're competitive" and do it when picks are most valued ([Footballguys](https://www.footballguys.com/article/2025-rebuilding-or-contending-the-dynasty-lifecycle)).
- Goalie volatility. Across three seasons the top-10 goalie lists showed "barely any overlap" ([Fantrax HQ](https://fantraxhq.com/?p=149855)). Analysts want about 1,500 shots before trusting a save percentage ([Hockey Graphs](https://hockey-graphs.com/2014/01/20/2013-2014-goaltending-performance-save-percentage-correlation/)), and figures of 2,000-3,000 shots appear elsewhere (snippet only). The "Zero G" approach is to avoid paying for goalies because "their value can both increase and decrease massively without warning", and to sell goalies after breakout seasons ([DobberHockey](https://dobberhockey.com/?p=77620)).

## 10. Startup and maintenance mistakes

- Drafting to an age ceiling. At least a third of managers in a typical pool do this, which leaves prime and veteran producers cheap ([Fantrax HQ](https://fantraxhq.com/?p=149855)).
- Switching strategy mid-draft: "Whatever strategy you choose, stick to it" ([Fantrax HQ](https://fantraxhq.com/?p=149855)).
- Reaching for goalies early, in startups and in rookie drafts ([RotoWire](https://www.rotowire.com/hockey/article/dynasty-league-strategy-58771)).
- Going all in too early, which is how teams fall into unplanned rebuilds ([DobberHockey](https://dobberhockey.com/?p=26758)).
- Half-hearted or abandoned rebuilds: "aborting your rebuild early will likely shorten your contention period" ([DobberHockey](https://dobberhockey.com/2013/07/18/rebuilding-to-contention-part-iii-tips-along-the-way)).
- Holding prospects past their value peak and selling young NHL players during the pre-breakout dip (section 9).
- Treating potential as production: "potential is just that, potential" ([RotoWire](https://www.rotowire.com/hockey/article/dynasty-league-strategy-58771)).

## How this applies to our dynasty league

Everything in this section is inference from the league settings and the sources above.

**Scope (confirmed by the owner on 2026-11-10, see the [README](README.md#league-facts-confirmed-from-the-app)): each Fantrax team keeps its top 3 players and everyone else is redrafted (10-round draft, 16-player roster).** Fantrax labels the league "Dynasty", but the multi-season reasoning in this file (aging, discounting, prospect holding, picks) applies to the three keeper slots only. For the other 13 roster spots, value is this season's production, as in redraft. The bullets below were written as if every player were kept and are corrected where that matters.

- Goalies carry a large share of the points. Illustrative arithmetic with our weights: a start with 28 saves, 2.8 goals against and a 50% win chance is worth 28 x 0.25 - 2.8 + 0.5 x 5 = 6.7 points before shutouts. A 100-point forward (40 goals, 60 assists, 30 power-play points, 280 shots) scores about 450 points in 82 games, or 5.5 per game before hits, blocks, penalty minutes and fights (the FT statistic is fights, not faceoffs). An ordinary goalie start is therefore worth about as much as an elite skater's game, and only two of twelve active slots are goalies. Two reliable starters on winning teams are core assets here in a way the general "wait on goalies" advice does not assume.
- That raises the price of goalies but not their predictability. The right response is to pay for volume and team quality (wins are 5, shutouts 7.5) on proven starters aged about 25 to 32, hold a third NHL goalie on reserve as insurance if one of the four reserve spots can spare him, and avoid goalie prospects who are years away. Only the best three players are kept, so a goalie's multi-year value matters only if he is likely to be one of the team's top three.
- Three moves a week limits streaming, so the four reserve spots (16-player cap with 12 active; the 6-reserve maximum is not reachable) have to supply schedule cover and injury cover. Each reserve spot given to a non-NHL prospect removes a usable game-day option.
- No minors slots. A prospect two or more years from the NHL costs one of only four reserve spots for the entire wait and has a meaningful chance of never producing (sections 4 and 8). Because just 3 players per team are kept, a prospect is worth holding only if he could plausibly be one of the team's top three in a future season. A contender should carry at most one such player; a rebuilder can carry two but should still prefer players already in the NHL and short of their breakout threshold.
- Shots at 0.5 and hits and blocks at 0.3 to 0.5 give steady value to high-volume young forwards (shot rate peaks around 22) and to physical defensemen whose points never arrive. The breakout-threshold wait is less costly for players who already fill these columns.
- Picks are tradeable (future picks exist for 2027 to 2029). With 10 rounds redrafting every non-keeper, a pick is a slot in a redraft of the non-keepers, not only in a rookie pool, so its value should come from the expected player at that slot (inference; the draft pool has not been checked) and the rookie-pool argument that follows is weaker than stated. As a starting point from the earlier reasoning, the rookie pool is shallow in practice because nobody can stash. Late picks are worth close to nothing; the first few picks hold nearly all the value. A contender should treat picks beyond the first round as trade currency, sold close to the draft when their price peaks.
- Head-to-head playoffs are noisy. Strength in the fantasy playoff weeks, particularly goalie starts those weeks, matters more than regular-season margin.

## Implications for the engine

Well supported by the sources:

1. Skater age multiplier should be nearly flat from about 23 to 29, not a sharp peak. Test: for the same projected points, the 3-year value of a 24-year-old and a 28-year-old forward should differ by less than about 15%.
2. Defensemen should hold value about two years longer than forwards at the old end (near-peak to 34 against 32) but mature later at the young end (400-game threshold against 200).
3. Goalies should have no youth bonus. Apply no growth term before about 25 and a decline from about 30 that steepens after 33-35. Test: an 18-to-22-year-old goalie with no NHL starts should never out-rank an established starter aged 26 to 31 in contend or balanced mode.
4. Pedigree should fall steeply with draft slot, closer to the market chart than the games chart: pick 30 at roughly 15-30% of pick 1, pick 60 at roughly 5-20%. Goalie pedigree should be much flatter by slot and weaker overall.
5. Pedigree should decay as evidence arrives. Once a skater passes his breakout threshold (200 games, or 400 for defensemen and forwards outside 5-10 to 6-2 and 171 to 214 pounds), draft slot should carry little or no weight. Before that threshold, do not mark a high-pedigree player down for ordinary production.
6. Prospect arrival time should be an explicit input: about 1-2 years for top-3 picks, 2-3 for the rest of the top 15, 3.5-4 for picks 16-30, longer for goalies, plus the games needed to reach the threshold.

Inference, to be tuned and back-tested:

7. Discount rate (for the three keeper slots; other players are valued for this season only, since non-keepers are redrafted). Published anchors are 8% a year (baseball finance) and the three-year rule. A defensible range for the weight on each successive season is about 0.75 to 0.90 in balanced mode, lower (0.5 to 0.65) in contend mode and near 1.0 with a reduced weight on the current season in rebuild mode. A three-year horizon matches the hockey heuristic; consider a small terminal value for players under 24 so the horizon cut-off does not erase the fourth and fifth years of a young star.
8. Roster-slot charge. With 16 roster spots, 12 active and 4 reserve, a long-term hold must beat a replacement reserve player for one of four spots and must have a realistic chance of being a top-3 keeper. For any player not expected to play NHL games this season, subtract the value of the best available replacement reserve player for each season of waiting. With no minors slots this charge should be large enough that a prospect two or more years away is positive only if he projects as a top-line scorer, and it should cap a contender at one such player (a rebuilder at two).
9. Bust probability. Multiply prospect future value by an arrival probability from draft slot and NHLe (for example the Bader table for forwards), not just by a pedigree scalar.
10. Market blend. Treat the market view as the price and the model as the value. The gap is the signal: model above market for players over 30 (buy when contending) and for post-hype players near their threshold; market above model for fresh draftees and hyped non-NHL goalies (sell).
11. Contend/rebuild recommendation should be driven by simulated playoff and title odds, not by roster age. Suggested rule: contend when projected to finish in the top four or when title odds are at or above the 8.3% baseline; rebuild only when playoff odds this season are low (for example under 20-25%) and the three-year outlook is also below baseline; otherwise balanced. Never recommend a rebuild to a team currently in a playoff position.
12. Win-now trade guard. In contend mode, flag any trade that gives up more than a set share of total dynasty value (for example 10-15%) for current-season points, and show the estimated change in title odds next to it. Weight added points in fantasy playoff weeks more than regular-season points.
13. Pick valuation. Value a future first-round pick from the owner's projected finish, apply the steep chart, and raise pick prices as the rookie draft approaches. Value picks after the first round near zero in this league, unless the 10-round redraft of non-keepers shows later slots return startable players (to be checked against the draft pool).

## Open questions / where sources disagree

- Peak age: 24-25 (value metrics, delta method) against 27-29 (points per game, regression). Which fits fantasy points under our scoring is untested; it should be measured on our own projections.
- Goalie aging: "varies little by age" (Brander) against steady decline from the mid-20s (garik16), with survivorship bias argued in both directions.
- Whether defensemen really peak later, or only take longer to earn offensive roles.
- Goalies in fantasy: the "Zero G" advice assumes typical scoring. Our win and shutout weights are heavy, so that advice may not transfer.
- Win-now against build-young: no outcome data was found for hockey or football. Harstad's and Sanderson's arguments are models, not results.
- No evidence was found on the accuracy of Pronman, Wheeler, Elite Prospects or DobberProspects rankings.
- The right fantasy discount rate has no published estimate; the range above is inference.
- League setup: the roster as described (6 F, 4 D, 2 G active plus 6 reserve plus 2 IR) looked inconsistent with a "16 max". It is consistent once the limits are read as separate caps: 12 active plus 4 reserve is 16 excluding IR, so only four reserve spots can be filled when the active lineup is full, and the roster-slot charge should use four. The faceoff stat worth 3 should also be identified, since it changes the value of centres. (Resolved after the research: the brief was wrong to call it faceoff-related. "FT" is fights, worth 3 points each, so it lifts enforcers, not centres. The roster limits are separate caps: at most 12 active, 6 reserve and 2 IR, with 16 in total excluding IR. See the [README](README.md#league-facts-confirmed-from-the-app).)
- Schuckers' data are from 1988-1997 drafts and Tulsky's from 2006-2012 trades; both predate current drafting practice.

## Sources

Pages read:

- https://www.eurekalert.org/news-releases/880909
- https://hockey-graphs.com/2017/03/23/a-new-look-at-aging-curves-for-nhl-skaters-part-1/
- https://hockey-graphs.com/2017/04/10/a-new-look-at-aging-curves-for-nhl-skaters-part-2/
- https://ar5iv.arxiv.org/html/2110.14017
- https://sabermetricmusings.blogspot.com/2020/07/nhl-skill-aging-curves-scoring-and-shot.html
- https://hockey-graphs.com/2014/03/21/how-well-do-goalies-age-a-look-at-a-goalie-aging-curve/
- https://hockey-graphs.com/2014/01/20/2013-2014-goaltending-performance-save-percentage-correlation/
- https://myslu.stlawu.edu/~msch/sports/Schuckers_NHL_Draft.pdf
- https://arxiv.org/abs/1411.5754
- https://www.broadstreethockey.com/post/nhl-draft-pick-value-trading-up/
- https://soundofhockey.com/2022/06/06/examining-the-value-of-nhl-draft-picks/
- https://soundofhockey.com/2022/06/10/understanding-the-market-for-future-nhl-draft-picks/
- https://dobberprospects.com/2020/05/16/nhl-draft-pick-probabilities/
- https://dobberprospects.com/?p=9103
- https://www.nhl.com/kraken/news/power-of-patience-nhl-draft-compared-to-nba-nfl-334766228
- https://thehockeynews.com/news/from-the-draft-to-the-nhl-a-round-by-round-look-at-the-leagues-skaters-and-goalies
- https://canucksarmy.com/news/a-recent-history-of-drafted-goaltenders
- https://flamesnation.ca/news/predicting-future-nhl-scoring-success-with-nhle-thresholds-related-factors
- https://hockeystats.com/methodology/nhle
- https://dobberhockey.com/2022/06/30/breakout-threshold-bt-using-career-games-played-to-determine-breakout-potential
- https://dobberhockey.com/?p=73269
- https://dobberhockey.com/2024/06/01/the-journey-top-36-draft-ranking-zero-picks-strategy-prospects-who-slipped-through-a-5-round-12-team-dynasty-draft
- https://dobberhockey.com/2022/06/04/the-journey-potential-stars-with-longer-timelines
- https://dobberhockey.com/?p=67718
- https://dobberhockey.com/?p=77620
- https://dobberhockey.com/?p=26758
- https://dobberhockey.com/2013/07/18/rebuilding-to-contention-part-iii-tips-along-the-way
- https://dobberhockey.com/?p=26057
- https://dobberhockey.com/?p=78760
- https://dobberhockey.com/2025/06/26/analytics-advantage-changes-to-nhl-draft-trends-including-player-size-production-by-overall-pick-and-more
- https://fantraxhq.com/?p=149855
- https://www.rotowire.com/hockey/article/dynasty-league-strategy-58771
- https://ca.sports.yahoo.com/news/fantasy-hockey-keeper-league-rules-rankings-strategy-advice-140748389.html
- https://blogs.fangraphs.com/an-update-to-prospect-valuation/
- https://www.dynastynerds.com/dynasty/dynasty-trade-secrets-understanding-draft-pick-values/
- https://www.footballguys.com/article/2023-dynasty-in-theory-68-when-to-rebuild (partly paywalled)
- https://www.footballguys.com/article/2023-dynasty-in-theory-65-chase-value-not-wins (partly paywalled)
- https://footballguys.com/article/2024-dynasty-in-theory-lets-talk-about-win-now-trades (partly paywalled)
- https://www.footballguys.com/article/2024-dynasty-in-theory-how-do-players-age (paywalled; not used for claims)
- https://www.footballguys.com/article/2025-rebuilding-or-contending-the-dynasty-lifecycle
- https://jakobsanderson.substack.com/p/market-principles-of-dynasty-part-2d7 (partly paywalled)

Pages that could not be opened (403 or 404) and are cited only where marked "snippet only" or not at all: Elite Prospects top-prospects methodology, InGoal Magazine goalie draft study, DynastyLeagueFootball startup strategies, TSN prime-years article, Medium career-high ages article. KeepTradeCut and The Athletic were not read.
