# 06 - Trade strategy and trade valuation

Researched 2026-11-10. Every factual claim links to the page it came from. Where a page could not be opened and the claim rests on a search-result excerpt only, the text says so.

## What this covers

This document covers how fantasy experts value trades, how public trade-value charts and calculators are built and where they fail, how to judge whether the other manager will say yes, when to trade, what risks a trade carries, and the platform rules on vetoes and collusion. It ends with concrete rules for our trade evaluator. Most published work is about football and baseball; hockey-specific evidence is called out where it exists. Dynasty is treated briefly because another document covers it.

## Key takeaways

- A trade must be judged on the **end-state rosters**, not on the players named in it. In an uneven trade (2-for-1), the side receiving one player also receives a free-agent pickup, and the side receiving two must drop someone. Leaving this out is described as the number one mistake in uneven trades ([Fantasy Alarm](https://www.fantasyalarm.com/articles/nfl/dynasty-leagues/fantasy-football-2024-dynasty-leagues-the-value-of-a-roster-spot/159860)).
- The value of the freed roster spot equals the value of the best available free agent, so it is **large in shallow leagues and small in deep ones**. Consolidation (trading two players for one better one) is therefore weaker in a 14-team league than in a 10-team league ([ESPN, Cockcroft](https://africa.espn.com/fantasy/baseball/story/_/page/mlbdk2k16_multitrade/fear-multi-player-trade-offers-mlb); [RotoStreetJournal](https://www.rotostreetjournal.com/2026/08/25/dynasty-trade-calculator-flaws-how-to-exploit/)).
- Consolidation is right when you are converting **bench players you do not start** into a lineup upgrade. It is a mistake when both outgoing players are starters, as in our goalie-plus-winger-for-centre case.
- Replacement level is **team-specific**. A strong team's worst rostered player is better than a weak team's, which changes what an incoming player is worth by as much as $10 on an auction scale in Cockcroft's example ([ESPN](https://africa.espn.com/fantasy/baseball/story/_/page/mlbdk2k16_multitrade/fear-multi-player-trade-offers-mlb)).
- Public calculators add a "package adjustment" or "star premium" because simple addition overvalues piles of mid players. Published sizes are small and vague: PFF says the side giving more players should pay 5-10% over the star's price ([PFF](https://www.pff.com/news/fantasy-football-pffs-trade-value-chart-for-redraft-leagues-ahead-of-week-9)).
- A lineup-based evaluator (ours) does not need a star premium bolted on, provided it models the waiver replacement, the forced drop and games actually startable. The premium is a patch for calculators that cannot see rosters.
- Owners systematically ask more for their own players than they would pay for them (the endowment effect). This is documented in laboratory markets and in real NBA, NFL and NHL draft-pick trades ([Kahneman, Knetsch and Thaler 1990](https://econpapers.repec.org/RePEc:ucp:jpolec:v:98:y:1990:i:6:p:1325-48); [Hobbs and Singh 2022](https://econpapers.repec.org/RePEc:bla:ecinqu:v:60:y:2022:i:4:p:1929-1942)). A "fair" offer by market value should be expected to be declined more often than not.
- Buying low and selling high has evidence behind it only when the signal is a percentage that is known to regress (shooting percentage, on-ice percentages), not when it is merely a hot or cold week ([CBS Sports](https://www.cbssports.com/nhl/news/the-roller-coaster-world-of-nhl-shooting-percentages/); [Losak, Weinbach and Paul 2023](https://econpapers.repec.org/RePEc:sae:jospec:v:24:y:2023:i:3:p:374-401)).
- After roughly a month, prior expectations and season-to-date results deserve about equal weight; a blend predicted better than either alone in football data ([Footballguys, Harstad](https://www.footballguys.com/article/2018-regression-alert-week-05)).
- Goalies are the least predictable asset. Save percentage is "a weakly repeatable skill" ([Naples, Gage and Nussbaum 2018](https://scholar.smu.edu/datasciencereview/vol1/iss2/12)), and several top-ranked goalies collapsed in 2025-26 ([RotoWire](https://www.rotowire.com/hockey/article/five-lessons-learned-from-2025-26-that-can-be-applied-to-next-year-111991)). Goalie values need wider error bars than skater values.
- Experts treat calculators as a way "to see how close a trade can be", not to declare a winner ([FantasyPros](https://www.fantasypros.com/2022/02/dynasty-roster-revolution-lets-talk-about-trade-calculators-2022-fantasy-football)). Our tool needs a "too close to call" verdict.
- Vetoes are for collusion. ESPN defines collusion as one team making moves "to benefit another team, without trying to improve its own position" and names one-sided trades as an example ([ESPN Fair Play](https://support.espn.com/hc/en-us/articles/115003845711-Fair-Play-and-Conduct)).

## 1. How experts value trades

**Value over replacement.** Replacement level is the production you can get for free, meaning the best player on waivers. A player's trade value is what he adds above that, not his raw points. PFF builds its chart by setting players "just outside likely starting range" to zero and valuing everyone else above that baseline from rest-of-season projections ([PFF](https://www.pff.com/news/fantasy-football-pffs-trade-value-chart-for-redraft-leagues-ahead-of-week-9)). Razzball defines replacement as a $1 player, the best one available on waivers (search-result excerpt of [Razzball](https://razzball.com/?p=4188716); page not opened).

Replacement level moves a lot with league size. RotoGraphs shows the same hitters swinging by "roughly $8-10" between a 10-team and a 15-team league, with players below replacement in the shallow league becoming clearly positive in the deep one ([RotoGraphs](https://fantasy.fangraphs.com/asset-valuation-part-1/)).

**The two questions in a 2-for-1.** RotoGraphs frames it as: "How good is the replacement player filling the vacated roster spot?" and "How good is the dropped player relative to the player replacing him?" ([RotoGraphs](https://fantasy.fangraphs.com/asset-valuation-part-1/)). Fantasy Alarm says to think "in terms of your END team": with full rosters, the side receiving two must drop a player, and "that value you are dropping to the wire might not even make the deal worth it" ([Fantasy Alarm](https://www.fantasyalarm.com/articles/nfl/dynasty-leagues/fantasy-football-2024-dynasty-leagues-the-value-of-a-roster-spot/159860)). DobberHockey's Fantasy Hockey Geek analyzer follows the same logic for hockey: it shows each team's totals before and after, and a player who must be dropped is entered as leaving the team ([DobberHockey](https://dobberhockey.com/2016/10/30/geek-of-the-week-trade-analyzer)).

**The "best player in the deal" heuristic.** This rule of thumb says the side that gets the best single player usually wins. The reasoning is that starting slots are finite, so one player's production cannot be replaced by two players who cannot both start. Cockcroft accepts the logic ("consolidating two players into one results in the acquisition of one player of superior ability") but argues managers have "been conditioned to reject 2-for-1 trades" too reflexively ([ESPN](https://africa.espn.com/fantasy/baseball/story/_/page/mlbdk2k16_multitrade/fear-multi-player-trade-offers-mlb)).

**When consolidation is right.**

- When the players you give are surplus you are "wasting", often bench players, and the freed spot goes to a useful pickup ([ESPN](https://africa.espn.com/fantasy/baseball/story/_/page/mlbdk2k16_multitrade/fear-multi-player-trade-offers-mlb)).
- In shallow leagues, where the waiver replacement is good. RotoStreetJournal says "elite assets break fantasy leagues. Depth only fills lineups", and limits that to "standard 9- or 10-team leagues" ([RotoStreetJournal](https://www.rotostreetjournal.com/2026/08/25/dynasty-trade-calculator-flaws-how-to-exploit/)).

**When consolidation is a mistake.**

- In deep leagues. The same article concedes "depth matters more in 12- or 14-team leagues" ([RotoStreetJournal](https://www.rotostreetjournal.com/2026/08/25/dynasty-trade-calculator-flaws-how-to-exploit/)). Cockcroft says receiving the two players is best "for teams that lack depth, especially in deeper leagues" ([ESPN](https://africa.espn.com/fantasy/baseball/story/_/page/mlbdk2k16_multitrade/fear-multi-player-trade-offers-mlb)).
- When your bench is thin. A team hit by injuries has the most to gain from receiving two players ([ESPN](https://africa.espn.com/fantasy/baseball/story/_/page/mlbdk2k16_multitrade/fear-multi-player-trade-offers-mlb)).
- When moves are limited. A hockey-specific guide says to "consider transaction limits and future schedule before acting" and to "count usable games, not only scheduled games" ([IceHockeyMan](https://icehockeyman.com/tag/two-for-one-fantasy-trade/); a lower-trust site, but the only hockey source found that addresses this directly).

The DobberHockey trading series itself does not address 2-for-1 deals; the consolidation point appears there only in a reader comment ([DobberHockey](https://dobberhockey.com/2016/10/19/10-tips-for-trading-in-your-fantasy-hockey-league-part-two)).

## 2. Trade value charts and calculators

**How they are built.** There are three families.

1. *Projection-based.* Rest-of-season projections minus a replacement baseline, expressed in dollars. PFF does this for football ([PFF](https://www.pff.com/news/fantasy-football-pffs-trade-value-chart-for-redraft-leagues-ahead-of-week-9)); RotoGraphs converts z-scores (how many standard deviations a player is above average in each category) into auction dollars for baseball ([RotoGraphs](https://fantasy.fangraphs.com/asset-valuation-part-1/)); Fantasy Hockey Geek applies Dobber's projections to your league's scoring to give a league-specific "FHG Value" ([DobberHockey](https://dobberhockey.com/2016/10/30/geek-of-the-week-trade-analyzer)).
2. *Market-based.* Values fitted to what people actually trade. The PeakedInHighSkool charts are calibrated against hundreds of real one-for-one trades, with running backs as the base currency and other positions expressed in running-back terms; the author restricts the data to one-for-one deals because uneven trades make the maths harder ([4for4](https://www.4for4.com/2022/preseason/fantasy-football-trade-value-models-what-why-how-and-who-cares)).
3. *Crowd-sourced.* KeepTradeCut asks users to rank three players as keep, trade or cut and derives values from the votes. Its FAQ says the "Value Adjustment" depends on the value gap, on "how much of a 'stud'" the players are and on the number of lesser players, and that it is reverse-engineered from the player that would be needed to even the deal (search-result excerpt of the [KeepTradeCut FAQ](https://keeptradecut.com/frequently-asked-questions?open=liquidity); the page could not be fetched, and no formula is published).

**Why values are non-linear.** A star is worth more than two mid players whose numbers add up to his, because a lineup has a fixed number of slots and the second mid player displaces someone you already start. Calculators handle this with an explicit correction. Dynasty League Football's "package adjustment" penalises the side receiving several assets because roster spots matter, and its author suggests turning it off in deep leagues (search-result excerpt of [DLF](https://dynastyleaguefootball.com/2020/08/04/dlf-dynasty-trade-analyzer-a-breakdown-and-example/); page returned 403). RotoViz exposes a "best player bonus" and a "quantity bonus per extra asset" as user settings ([RotoViz](https://apps.rotoviz.com/tradeCalc/)). PFF gives the only plain number found: a premium of "5-10% over the stud player's cost" ([PFF](https://www.pff.com/news/fantasy-football-pffs-trade-value-chart-for-redraft-leagues-ahead-of-week-9)).

RotoGraphs shows the same curve in draft picks: a first-round pick is worth about twice a third-round pick, while the gap from round three to round five is only about $5 ([RotoGraphs](https://fantasy.fangraphs.com/asset-valuation-part-1/)).

**Known weaknesses.**

- They cannot see team context, roster needs or league rules, and different calculators disagree ([FantasyPros](https://www.fantasypros.com/2022/02/dynasty-roster-revolution-lets-talk-about-trade-calculators-2022-fantasy-football)).
- They struggle "with cross-positional values and with quantifying value at the top and bottom" ([4for4](https://www.4for4.com/2022/preseason/fantasy-football-trade-value-models-what-why-how-and-who-cares)).
- The "consolidation trap": summing values can make three bench players equal one elite player ([RotoStreetJournal](https://www.rotostreetjournal.com/2026/08/25/dynasty-trade-calculator-flaws-how-to-exploit/)).
- Liquidity (how easily an asset can be traded again) is ignored; a pick that every team wants is treated like an ageing veteran few teams want ([RotoStreetJournal](https://www.rotostreetjournal.com/2026/08/25/dynasty-trade-calculator-flaws-how-to-exploit/)).
- Scarcity created by league settings is under-weighted: "The calculator sees equal points; it doesn't see the positional monopoly" ([RotoStreetJournal](https://www.rotostreetjournal.com/2026/08/25/dynasty-trade-calculator-flaws-how-to-exploit/)). The hockey analogue is the goalie slot.

## 3. Positional need and lineup fit

The same player is worth different amounts to different teams because each team replaces a different player when he arrives. Cockcroft: the first-place team "is going to have a considerably higher team 'replacement level' than the one in last", so "simply adding dollar values together is not enough" ([ESPN](https://africa.espn.com/fantasy/baseball/story/_/page/mlbdk2k16_multitrade/fear-multi-player-trade-offers-mlb)).

Trading from depth means giving players at a position where your next man is nearly as good, to a team where they would start. FantasyPros: give "something you have a lot of so your team can get better after the trade", and look for a partner who is deep where you are weak and weak where you are deep ([FantasyPros](https://www.fantasypros.com/2024/07/fantasy-football-trade-strategy-advice)). ESPN's hockey column gives the same advice for a manager with "an excess of quality centers" ([ESPN](https://africa.espn.com/fantasy/hockey/story/_/id/31134894/top-ten-fantasy-hockey-trades-need-make-win-now)). DobberHockey's first tip is to "look for handcuffed teams": managers whose draft left a hole only a trade can fix, especially in deep leagues with few free agents ([DobberHockey](https://dobberhockey.com/2016/10/12/tips-for-trading-in-your-fantasy-hockey-league)).

The Fantasy Hockey Geek example shows why league-specific value matters: a goalie projected for more raw points than a defenceman had less than half his league value (43.6 against 88.7), and the writer still passed on the deal because of his own goalie depth ([DobberHockey](https://dobberhockey.com/2016/10/30/geek-of-the-week-trade-analyzer)).

## 4. Buy low, sell high

"Buy low" means acquiring a player whose results are below his true level; "sell high" is the reverse. The idea only works if the gap is luck.

**Signals with evidence.**

- *Shooting percentage* (goals divided by shots). Of the 50 forwards with the highest shooting percentage in 2011-12, 42 dropped the next season, by more than 5 percentage points on average ([CBS Sports](https://www.cbssports.com/nhl/news/the-roller-coaster-world-of-nhl-shooting-percentages/)). DobberHockey compares current shooting percentage to career norms, checks on-ice shooting percentage (teammates' finishing while the player is on the ice), and holds shot volume steady when projecting ([DobberHockey](https://dobberhockey.com/2024/11/03/geek-of-the-week-stock-watch-buy-low-sell-high)).
- *IPP* (individual points percentage: the share of goals scored while a player is on the ice on which he gets a point; this definition is standard usage and was not spelled out in the pages read). Roos uses IPP and team shooting percentage against a player's own history ([DobberHockey](https://dobberhockey.com/2016/10/19/10-tips-for-trading-in-your-fantasy-hockey-league-part-two)).
- *Role.* "Opportunity and circumstances can trump talent"; no forward with 80 or more points since 2010-11 averaged under 3:08 of power-play time ([DobberHockey part one](https://dobberhockey.com/2016/10/12/tips-for-trading-in-your-fantasy-hockey-league); [part two](https://dobberhockey.com/2016/10/19/10-tips-for-trading-in-your-fantasy-hockey-league-part-two)). A role change is a real change, not luck.

**Signals without evidence.** A hot week by itself. A peer-reviewed study of daily fantasy baseball found "no evidence of a hot hand effect, and no evidence of a profitable hot hand strategy", yet players build it into lineups anyway ([Losak, Weinbach and Paul 2023](https://econpapers.repec.org/RePEc:sae:jospec:v:24:y:2023:i:3:p:374-401)). That belief is what makes selling high possible: the edge comes from the other manager's bias.

**How much to trust the season so far.** In football, preseason draft position and first-four-game results correlated about equally with rest-of-season results (0.557 and 0.555 on average over 2010-2017), and their average did better (0.682 in 2016, 0.608 in 2017) ([Footballguys](https://www.footballguys.com/article/2018-regression-alert-week-05)). No equivalent hockey study was found.

Roos adds that "buy low, sell high" is common knowledge; the less-known edge is "cost vs. value", meaning players whose price is inflated by name, market size or draft position ([DobberHockey](https://dobberhockey.com/2016/10/12/tips-for-trading-in-your-fantasy-hockey-league)), and warns "don't bank on breakouts" ([DobberHockey](https://dobberhockey.com/2016/10/19/10-tips-for-trading-in-your-fantasy-hockey-league-part-two)).

## 5. The other side of the table

**Documented biases.**

- *Endowment effect*: valuing something more because you own it. In the original experiments, "measures of willingness-to-accept greatly exceed measures of willingness-to-pay", and far fewer mugs traded than theory predicts ([Kahneman, Knetsch and Thaler 1990](https://econpapers.repec.org/RePEc:ucp:jpolec:v:98:y:1990:i:6:p:1325-48)). The authors tie it to *loss aversion*: "the disutility of giving up an object is greater than the utility associated with acquiring it" ([Kahneman, Knetsch and Thaler 1991](https://www.aeaweb.org/articles?id=10.1257/jep.5.1.193)). Professional teams show it too: a study of NBA, NFL and NHL draft-pick trades found "strong evidence of the endowment effect overall" ([Hobbs and Singh 2022](https://econpapers.repec.org/RePEc:bla:ecinqu:v:60:y:2022:i:4:p:1929-1942)).
- In fantasy terms, Harstad calls this a personal bid-ask spread: the price you would pay for a player "is invariably lower" than the price you would need to give him up, which is why two managers who value two players equally will both refuse a straight swap ([Footballguys](https://www.footballguys.com/article/2025-dynasty-in-theory-week-13)). RotoGraphs' test: rename the player and ask whether you would pay that price ([RotoGraphs](https://fantasy.fangraphs.com/the-endowment-effect-is-your-enemy)).
- *Anchoring* (over-relying on the first number heard), *hot-hand belief*, *reactive devaluation* (discounting an offer because of who sent it) and *sunk cost* are described for hockey trades by DobberHockey ([DobberHockey](https://dobberhockey.com/2023/08/27/the-journey-cognitive-biases-in-trade-negotiations)).
- *Name value*: players on Original Six and big-market teams and top jersey sellers cost more than they produce ([DobberHockey](https://dobberhockey.com/2016/10/12/tips-for-trading-in-your-fantasy-hockey-league)).

No peer-reviewed study measuring these biases in season-long fantasy trades was found; the fantasy evidence is practitioner writing.

**Estimating acceptance.** One tool publishes its approach: 47 features including market values, player age, trade structure and, most important, that manager's past trading behaviour. It reads a 74% score as a base rate, not a prediction, separates "accept outright" from "worth sending to start a conversation", and admits it only sees accepted trades because the platform does not expose rejected ones ([ARC Dynasty FAQ](https://swaggyj0.github.io/arc-dynasty/dynastyiq-faq.html); a single vendor's description, accuracy unverified).

**Negotiation practice.**

- Open in your favour but within reason, since the first number anchors; an absurd opener ends the talk ([The Fantasy Footballers](https://www.thefantasyfootballers.com/analysis/negotiation-strategy-five-tips-for-your-fantasy-football-trades/)).
- Offers you would not accept yourself teach the other manager to reject everything you send ([DobberHockey](https://dobberhockey.com/2023/08/27/the-journey-cognitive-biases-in-trade-negotiations)).
- Offered players must be "at least one tier above the players on the waiver wire" ([PFF](https://www.pff.com/news/30-ways-to-win-trade)).
- Do not include injured players in your offer; to trade depth for a star, "flood your trade partner's starting lineup with significant value" and pitch when they are desperate ([PFF](https://www.pff.com/news/30-ways-to-win-trade)).
- Set a walk-away point first; trades are repeated games and reputation carries over ([The Fantasy Footballers](https://www.thefantasyfootballers.com/analysis/negotiation-strategy-five-tips-for-your-fantasy-football-trades/)).
- Expect several rounds; "if they counter you, counter them back" ([FantasyPros](https://www.fantasypros.com/2024/07/fantasy-football-trade-strategy-advice)).

## 6. Timing

- **Early season.** Small samples are noisy, but they are not worthless: by four games, football results carried about as much information as preseason rank ([Footballguys](https://www.footballguys.com/article/2018-regression-alert-week-05)). The sound response is to blend, and to trade against managers who do not.
- **Injuries on the other team.** Offers of depth work best right after the other manager suffers a significant injury ([ESPN](https://africa.espn.com/fantasy/baseball/story/_/page/mlbdk2k16_multitrade/fear-multi-player-trade-offers-mlb)).
- **Playoff schedules.** NHL teams play different numbers of games in each fantasy playoff week (some only twice), but ESPN's guide says managers should avoid swapping out elite players just for a better schedule; the schedule is a tiebreaker and a streaming tool ([ESPN](https://africa.espn.com/fhl/story/_/id/44244119/fantasy-hockey-playoffs-ultimate-strategy-guide)).
- **Deadlines and eliminated teams.** Late deals usually send future assets from contenders to eliminated teams for veterans. Harstad argues this is "a mechanism for parity" and that deadlines exist to stop eliminated teams acting as kingmakers ([Footballguys](https://www.footballguys.com/article/HarstadDiT11)). In a redraft league an eliminated team has nothing to gain, so such trades draw collusion scrutiny (section 8). In one-year leagues Roos warns against buying older players around the holidays ([DobberHockey](https://dobberhockey.com/2016/10/12/tips-for-trading-in-your-fantasy-hockey-league)).

## 7. Risk in trades

- **Goalies.** Save percentage is "a weakly repeatable skill and predictor of future performance" ([Naples, Gage and Nussbaum 2018](https://scholar.smu.edu/datasciencereview/vol1/iss2/12)). Hockey Graphs says it "takes a large sample to remove the noise" and cites a figure of about 1,500 shots against (the article's own text is inconsistent on the exact number) ([Hockey Graphs](https://hockey-graphs.com/2014/01/20/2013-2014-goaltending-performance-save-percentage-correlation/)). In 2025-26 league save percentage fell to .896, and Vasilevskiy, Oettinger and Hellebuyck finished far below their preseason ranks while volume goalies on middling teams topped the points table ([RotoWire](https://www.rotowire.com/hockey/article/five-lessons-learned-from-2025-26-that-can-be-applied-to-next-year-111991)).
- **Age.** Skaters generally peak between 24 and 28; in keeper leagues, tolerance for older players "should correlate to the extent your team is in win now mode" ([DobberHockey](https://dobberhockey.com/2016/10/12/tips-for-trading-in-your-fantasy-hockey-league)).
- **Concentration.** A 2-for-1 puts more of your points in one body. One analyzer's own caveat: the tool cannot see how thin your bench is, and "if you are one injury from disaster, keep the depth" (search-result excerpt of [grand-screen.com](https://grand-screen.com/tools/fantasy-trade-value-calculator/); low-trust, page not opened). No quantified study of this was found.
- **Injured players.** Price an injured player as his healthy value discounted by his return timeline ([PFF](https://www.pff.com/news/30-ways-to-win-trade)).

## 8. Fairness, vetoes and collusion

**ESPN.** Collusion "occurs when one team makes moves to benefit another team, without trying to improve its own position", and "one-sided trades are an example of collusive transactions"; dumping players to undermine a league can get a manager removed ([ESPN Fair Play](https://support.espn.com/hc/en-us/articles/115003845711-Fair-Play-and-Conduct)). Every manager except the two involved can vote; a trade is cancelled at 4 of 10 votes or 50% of uninvolved teams; in League Manager leagues the manager "has final say" ([ESPN veto help](https://support.espn.com/hc/en-us/articles/4669766880532-Veto-or-Protest-a-Trade)). The review period can be one, two or three days or none ([ESPN trade review help](https://support.espn.com/hc/en-us/articles/360000071412-Trade-Review)). The default length was reported differently in two readings of that page, so check the league's own settings.

**Fantrax.** Four trade-voting options: None (executes on acceptance), Owners (vetoed if a minimum number of objections arrives within a set number of days), Commissioner (private leagues; unlimited time to approve or reject) and Owners & Commissioner ([FantraxHQ](https://www.fantraxhq.com/?p=92124), a 2021 third-party guide). Fantrax's own FAQ could not be opened; thresholds are set per league.

**Norms.** In a survey of 17 fantasy writers, 16 named collusion as a legitimate veto reason and 10 named it as the only one; three accepted "extremely one-sided" ([Hashtag Basketball](https://hashtagbasketball.com/fantasy-basketball/content/fantasy-experts-on-the-right-reasons-to-veto-a-trade)). This sits in tension with ESPN's wording, which treats one-sidedness itself as evidence of collusion.

## 9. Dynasty in brief

- Picks are currency whose value falls steeply after the top few; a pick a year further away is worth about 20-30 draft spots less ([DobberHockey, MacLean](https://dobberhockey.com/hockey-home/capped/capped-valuing-draft-picks-in-all-rounds); a rule of thumb, not a fitted model).
- Prospects typically take five to six years from draft to fantasy upside, so first-round farm picks are often worth more as trade chips than as selections ([DobberHockey, Gehrels](https://dobberhockey.com/?p=80338); argued from examples, no hit-rate data).
- Contenders pay future assets for present production and rebuilders do the reverse; judging such a trade in isolation produces "fake losses" ([RotoStreetJournal](https://www.rotostreetjournal.com/2026/08/25/dynasty-trade-calculator-flaws-how-to-exploit/); [ESPN](https://africa.espn.com/fantasy/hockey/story/_/id/31134894/top-ten-fantasy-hockey-trades-need-make-win-now)).

## 10. What a good automated recommendation looks like

Drawn from the sources above:

- Show closeness, not a winner: "what calculators are truly for is to see how close a trade can be" ([FantasyPros](https://www.fantasypros.com/2022/02/dynasty-roster-revolution-lets-talk-about-trade-calculators-2022-fantasy-football)).
- Show before-and-after team totals for both sides, including any forced drop ([DobberHockey](https://dobberhockey.com/2016/10/30/geek-of-the-week-trade-analyzer)).
- Present acceptance as a base rate and separate "likely accepted" from "worth sending" ([ARC Dynasty FAQ](https://swaggyj0.github.io/arc-dynasty/dynastyiq-faq.html)).
- Treat the number as "a starting point in a conversation" ([4for4](https://www.4for4.com/2022/preseason/fantasy-football-trade-value-models-what-why-how-and-who-cares)). Even the Fantasy Hockey Geek writer overrode the tool on keeper status and depth ([DobberHockey](https://dobberhockey.com/2016/10/30/geek-of-the-week-trade-analyzer)).

Failure modes: summing values across uneven trades; ignoring the waiver pool and the forced drop; one replacement level for every team; point estimates with no uncertainty; evaluating one trade without the team's plan; under-weighting scarce positions.

## How this applies to our two leagues

**ESPN, 14-team redraft (9 F, 5 D, 1 UTIL, 2 G, 5 bench, 2 IR; 2 moves per week).** With 14 teams and 22 active-plus-bench spots, roughly 308 players are rostered, so the waiver pool is thin and the freed spot in a 2-for-1 is worth little. This is the "deep league" case in which sources favour keeping depth. The two-move weekly cap lowers the value of an open spot further, because the usual justification for an empty spot (streaming players with extra games) is rationed. Only two goalies start, and goalie replacement level on waivers in a 14-team league is poor, so trading a top goalie leaves a hole a pickup cannot fill. A trade that sends two starters (a top goalie and a strong winger) for one centre has to clear a high bar: the centre's gain over the forward he displaces must exceed the winger's loss plus the whole drop from the top goalie to the replacement goalie. Consolidation here only makes sense when the outgoing pieces are bench players. Trades with eliminated teams late in the year will be looked at under ESPN's one-sided-trade wording.

**Fantrax, 12-team dynasty (6 F, 4 D, 2 G, 6 reserve, 2 IR; picks tradeable).** Only 12 skaters and goalies start per team, so star power matters more per slot and consolidation is more defensible than on ESPN, with reserve spots holding prospects that have future value but no present points. Picks give a third currency to balance deals without touching the lineup. Every trade has a time dimension: the right verdict depends on whether we are contending this season. Goalie volatility argues against paying a premium, in future assets, for a goalie on the strength of one season.

## Implications for the engine

Labels: **[S]** well supported by the sources above; **[I]** our own inference.

1. **Evaluate end-state rosters. [S]** For every trade, build both post-trade rosters: fill each freed spot with the best available free agent for that team's need, and for the side that gains players, remove its least valuable player. The verdict comes from optimal lineups on those rosters. *Test:* a 2-for-1 scored with and without the waiver fill must differ by exactly the fill's lineup contribution.
2. **Do not credit a freed spot with more than the waiver pool offers. [S]** The credit is the marginal contribution of the actual best free agent, which is zero if he would not crack the lineup or cover a gap. **[I]** With a two-moves-per-week cap, do not add any "streaming" value to an open spot unless a move budget is explicitly modelled.
3. **Use team-specific replacement level. [S]** Value each player by what he adds over the player he displaces on that particular team, for both sides.
4. **Score in total expected points over the horizon, not points per game. [I]** Per-game rates hide games played, goalie start share, schedule density and nights when a full lineup leaves players unused. Simulate daily lineups over the remaining schedule ("count usable games").
5. **Check each position's slot separately. [I]** A trade that empties a scarce slot (G) must show the replacement actually used in that slot. *Test:* trading away a starting goalie must lower projected goalie-slot points unless an equal goalie arrives.
6. **Depth and injury stress test. [I]** Re-score the post-trade roster with each of its top players removed for a stretch; if the trade's gain turns negative under one plausible injury, flag "concentrates risk". Sources say this matters but give no number.
7. **Carry uncertainty on every player value. [S for the need, I for the method]** Use projection intervals, wider for goalies and for players with small samples. Report the gain as a range.
8. **"Too close to call" verdict. [S for the concept, I for the threshold]** If the range of the gain includes zero, or the gain is under a minimum meaningful size (to be tuned; start near the week-to-week noise in our own lineup projection), say so instead of accept or decline.
9. **Blend prior and current-season results. [S in football, I for hockey]** Do not let a hot or cold month move a value by more than a blend supports. Regress shooting percentage and on-ice percentages toward career norms before valuing; keep shot volume and role.
10. **Flag regression-driven trades. [S]** Mark "sell high" when current percentages are well above the player's norm and role is unchanged, "buy low" for the reverse, and show the stat behind the flag.
11. **Acceptance needs an ownership premium. [S for the bias, I for the size]** The other manager must gain on their own roster by their perceived values, with a margin for the endowment effect. An offer that is exactly even by market value should be scored as more likely declined than accepted. PFF's 5-10% premium on the side giving more players is a starting margin for uneven offers.
12. **Perceived value is not our value. [S]** Keep two numbers per player: our projection-based value and a market-perception value (draft position, rostered percentage, name, recent hot streak). Profit lives in the gap; acceptance depends on the second.
13. **Counterparty fit. [S]** Raise acceptance when the offer fills a hole in their starting lineup or follows an injury; lower it when incoming players are barely above their waiver options or are injured.
14. **Never recommend an offer the engine would decline in the other seat. [S]** Repeated lowballs cause reactive devaluation. Also flag trades so lopsided they invite a veto under ESPN's one-sided-trade wording.
15. **No bolt-on star premium. [I]** If rules 1-5 hold, non-linearity emerges from lineup slots. Add an explicit premium only if back-testing shows the lineup model still undervalues stars.
16. **Dynasty verdicts need a stated mode. [S]** On Fantrax, require contend or rebuild as an input and report this-season and future value separately; do not net them into one number silently.
17. **Regression test for the known bug. [I]** Keep the goalie-plus-winger-for-centre case as a fixture; the engine must not return "accept" unless the forward upgrade exceeds the winger loss plus the goalie-slot drop.

## Open questions / where sources disagree

- **How much is a star worth beyond his points?** RotoStreetJournal says elite assets decide leagues; Cockcroft says the 2-for-1 is unfairly dismissed and depth is valuable in deep leagues. A RotoWire analysis reportedly found value-over-baseline falling roughly linearly across draft slots (search excerpt only, not opened). No source gives an empirically fitted premium.
- **Hot streaks.** Losak and colleagues find no hot hand in fantasy baseball; a Stanford study by Green and Zwiebel reportedly found recent at-bats predictive (secondhand, not verified here).
- **Goalie predictability.** The SMU paper calls save percentage weakly repeatable; Hockey Graphs calls it "highly repeatable and predictive ... once sufficient sample size is met". Both imply a single season is too little.
- **Vetoes.** Most experts say collusion only; ESPN's policy text counts one-sided trades as collusive.
- **Trade deadlines and eliminated teams.** Harstad sees late trades as healthy; many leagues restrict them.
- **Not found:** a hockey trade-value chart with a published method; a hockey study of early-season stabilisation; any measurement of how often fair offers are accepted in season-long leagues; the size of the endowment premium in fantasy.

## Sources

Read in full or in part:

- https://africa.espn.com/fantasy/baseball/story/_/page/mlbdk2k16_multitrade/fear-multi-player-trade-offers-mlb
- https://fantasy.fangraphs.com/asset-valuation-part-1/
- https://fantasy.fangraphs.com/the-endowment-effect-is-your-enemy
- https://www.fantasyalarm.com/articles/nfl/dynasty-leagues/fantasy-football-2024-dynasty-leagues-the-value-of-a-roster-spot/159860
- https://www.pff.com/news/fantasy-football-pffs-trade-value-chart-for-redraft-leagues-ahead-of-week-9
- https://www.pff.com/news/30-ways-to-win-trade
- https://www.4for4.com/2022/preseason/fantasy-football-trade-value-models-what-why-how-and-who-cares
- https://www.fantasypros.com/2022/02/dynasty-roster-revolution-lets-talk-about-trade-calculators-2022-fantasy-football
- https://www.fantasypros.com/2024/07/fantasy-football-trade-strategy-advice
- https://www.rotostreetjournal.com/2026/08/25/dynasty-trade-calculator-flaws-how-to-exploit/
- https://apps.rotoviz.com/tradeCalc/
- https://swaggyj0.github.io/arc-dynasty/dynastyiq-faq.html
- https://www.thefantasyfootballers.com/analysis/negotiation-strategy-five-tips-for-your-fantasy-football-trades/
- https://www.footballguys.com/article/2025-dynasty-in-theory-week-13 (free portion)
- https://www.footballguys.com/article/2025-regression-alert-week-14 (free portion)
- https://www.footballguys.com/article/2018-regression-alert-week-05
- https://www.footballguys.com/article/HarstadDiT11
- https://dobberhockey.com/2016/10/12/tips-for-trading-in-your-fantasy-hockey-league
- https://dobberhockey.com/2016/10/19/10-tips-for-trading-in-your-fantasy-hockey-league-part-two
- https://dobberhockey.com/2016/10/30/geek-of-the-week-trade-analyzer
- https://dobberhockey.com/2023/08/27/the-journey-cognitive-biases-in-trade-negotiations
- https://dobberhockey.com/2024/11/03/geek-of-the-week-stock-watch-buy-low-sell-high
- https://dobberhockey.com/hockey-home/capped/capped-valuing-draft-picks-in-all-rounds
- https://dobberhockey.com/?p=80338
- https://icehockeyman.com/tag/two-for-one-fantasy-trade/
- https://africa.espn.com/fhl/story/_/id/44244119/fantasy-hockey-playoffs-ultimate-strategy-guide
- https://africa.espn.com/fantasy/hockey/story/_/id/31134894/top-ten-fantasy-hockey-trades-need-make-win-now
- https://www.rotowire.com/hockey/article/five-lessons-learned-from-2025-26-that-can-be-applied-to-next-year-111991 (free portion)
- https://www.cbssports.com/nhl/news/the-roller-coaster-world-of-nhl-shooting-percentages/
- https://hockey-graphs.com/2014/01/20/2013-2014-goaltending-performance-save-percentage-correlation/
- https://hockey-graphs.com/2016/08/19/quick-post-do-past-sv-variables-predict-future-sv-variables/ (about skaters' on-ice save percentage; not used)
- https://scholar.smu.edu/datasciencereview/vol1/iss2/12 (abstract)
- https://econpapers.repec.org/RePEc:ucp:jpolec:v:98:y:1990:i:6:p:1325-48 (abstract)
- https://www.aeaweb.org/articles?id=10.1257/jep.5.1.193 (abstract)
- https://econpapers.repec.org/RePEc:bla:ecinqu:v:60:y:2022:i:4:p:1929-1942 (abstract)
- https://econpapers.repec.org/RePEc:sae:jospec:v:24:y:2023:i:3:p:374-401 (abstract)
- https://support.espn.com/hc/en-us/articles/115003845711-Fair-Play-and-Conduct
- https://support.espn.com/hc/en-us/articles/360000064711-Fair-Play-Conduct-Collusive-Transactions
- https://support.espn.com/hc/en-us/articles/4669766880532-Veto-or-Protest-a-Trade
- https://support.espn.com/hc/en-us/articles/360000071412-Trade-Review
- https://www.fantraxhq.com/?p=92124
- https://hashtagbasketball.com/fantasy-basketball/content/fantasy-experts-on-the-right-reasons-to-veto-a-trade

Seen only as search-result excerpts (the page could not be opened; treat claims drawn from them as unverified):

- https://keeptradecut.com/frequently-asked-questions?open=liquidity
- https://dynastyleaguefootball.com/2020/08/04/dlf-dynasty-trade-analyzer-a-breakdown-and-example/
- https://razzball.com/?p=4188716
- https://grand-screen.com/tools/fantasy-trade-value-calculator/
- https://www.fantrax.com/newui/fantasy/faq.go
