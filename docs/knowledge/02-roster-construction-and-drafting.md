# 02 - Roster construction and drafting

Researched 2026-11-10. Sources were read through a web-fetch tool that summarises pages, so every quotation below should be re-checked against the page before it is quoted anywhere public. Numbers that are my own arithmetic are labelled "own inference".

## What this covers

This note collects what fantasy hockey experts say about building a roster: how to split draft picks between forwards (F), defensemen (D) and goalies (G), how scarce each position really is, how many goalies to carry, how to use the bench, whether to build around a few stars or a balanced group, how far to trust rookies and preseason rankings, and the mistakes that recur. It then translates that into the owner's two head-to-head points leagues (ESPN 14-team redraft, Fantrax 12-team dynasty) and into rules the engine can test. Both drafts are already done for this season, so the practical use is in-season: the same logic governs waiver adds, trades, keeper choices and next year's draft.

## Key takeaways

- **Value is measured against replacement, not in raw points.** "Replacement level" means the best player freely available on waivers at a position; value over replacement (VORP) is a player's projected points minus that baseline. The baseline is found by multiplying roster spots per position by the number of teams ([Yahoo/Groot Nibbelink 2022](https://sports.yahoo.com/fantasy-hockey-value-players-positions-based-on-league-settings-122553499.html), [DobberHockey/Brudner 2023](https://dobberhockey.com/?p=79234)).
- **Deeper leagues lower the replacement level, which makes above-replacement players worth more** ([DobberHockey](https://dobberhockey.com/?p=79234), [FanGraphs/Vander Lugt 2026](https://fantasy.fangraphs.com/asset-valuation-pt-4-messing-with-the-draft/)).
- **Defense drops off faster than forward.** Roughly 25-30 defensemen carry real fantasy value because each NHL team puts about one defenseman and four forwards on its first power-play unit ([Yahoo](https://sports.yahoo.com/fantasy-hockey-value-players-positions-based-on-league-settings-122553499.html)).
- **Goalie results are hard to predict.** One analysis found a year-to-year correlation of only 0.12 for goalie performance ([JFresh 2020](https://jfresh.substack.com/p/why-goaltending-is-basically-random)). This is the core of the "Zero G" argument for not paying early picks for goalies ([Yahoo/Groot Nibbelink 2022](https://ca.sports.yahoo.com/fantasy-hockey-zerog-strategy-why-you-shouldnt-draft-goalie-early-152209568.html)).
- **Experts disagree on goalie timing.** Zero G says wait; ESPN's Victoria Matiash says lock up a one-two punch early in deeper leagues ([ESPN 2025](https://africa.espn.com/fhl/story/_/id/46216225/espn-fantasy-hockey-goalie-draft-stars-sleepers-best-picks)); RotoWire says waiting works best in small leagues ([RotoWire 2025](https://www.rotowire.com/hockey/article/top-fantasy-hockey-draft-strategies-2023-96654)).
- **Workhorse goalies are disappearing.** In 2025-26 only one goalie reached 60 starts and 18 others played 50 or more games ([ESPN/Clark 2026](https://africa.espn.com/nhl/story/_/id/49029525/nhl-2026-playoffs-stanley-cup-final-goaltender-tandems-bussi-andersen-hurricanes)), so goalie volume is itself scarce.
- **Goalies are the one position that can score negative.** "Being wrong about one goaltender can negate being right about three skaters when streaming" ([ESPN/Allen 2022](https://africa.espn.com/fhl/story/_/id/35196481/espn-nhl-how-add-drop-stream-max-your-team)).
- **Transaction limits kill streaming.** "Limits to roster moves eliminate the advantage of off-night streaming" ([ESPN/Allen 2022](https://africa.espn.com/fhl/story/_/id/35196481/espn-nhl-how-add-drop-stream-max-your-team)). Both of our leagues have tight limits (2 and 3 moves per week), so roster quality and schedule fit matter more than churn.
- **Scoring weights decide how much "bangers" matter.** ESPN's own guide: do not chase a hitter "if hits are only each worth 0.1 points", but re-rank defensemen when blocked shots pay ([ESPN/Matiash 2023](https://global.espn.com/fantasy/hockey/story/_/id/38373882/espn-nhl-2023-draft-how-play-win-fantasy-hockey-league-pro)).
- **In redraft, proven veterans beat rookies and breakout guesses on average; in dynasty, elite youth is worth paying for** ([RotoWire 2025](https://www.rotowire.com/hockey/article/top-fantasy-hockey-draft-strategies-2023-96654), [DobberHockey/Collins 2021](https://dobberhockey.com/2021/06/21/top-10-fantasy-hockey-lessons-learned-this-year), [RotoWire/Eagleson 2026](https://www.rotowire.com/hockey/article/five-lessons-learned-from-2025-26-that-can-be-applied-to-next-year-111991)).
- **An injured player in a free IR slot costs nothing and should be valued on his full-season projection plus replacement-level fill-in; when IR slots are short he is worth less** ([DobberHockey/Brudner 2023](https://dobberhockey.com/?p=79234)).
- **I found no independent multi-season study of how accurate fantasy hockey preseason rankings are.** The evidence is anecdotal and should be treated that way.

## 1. Draft strategy: allocating picks across F, D and G

**Start with forwards, but know your format.** ESPN's Victoria Matiash advises loading up on elite forwards first and leaving goalies "until the third round, at minimum, depending on league size", while taking elite defensemen early only when it does not cost a top forward ([ESPN 2023](https://global.espn.com/fantasy/hockey/story/_/id/38373882/espn-nhl-2023-draft-how-play-win-fantasy-hockey-league-pro)). Fantrax HQ's Logan Doyle describes three opening plans (one F, one D, one G; "build from the net out"; or three forwards) and prefers three forwards, typically holding six forwards, one defenseman and one goalie after eight rounds ([Fantrax HQ 2024](https://fantraxhq.com/?p=149074)). The opposite view, "building from the net out", argues for two goalies and two defensemen in the first six rounds because goalie categories are about 40 percent of a head-to-head categories league ([theScore/Wegman, undated, about 2016-17](https://thescore.com/news/1089807)). That argument is built on category leagues, where each goalie category counts as much as a skater category; it does not carry over directly to points leagues, where a goalie is worth exactly the points he scores.

**The goalie-timing debate.** "Zero G" (coined by Nate Groot Nibbelink of Apples & Ginos) means not spending early picks on goalies. Its case: goalies are less predictable than skaters, early-round skaters "are some of the safest plays in the game", and useful goalies appear on waivers every year. In his 2020-21 study, drafting goalies ranked 1-14 by average draft position gave "nearly no benefit" in end-of-season finish over those ranked 15-28; he also reports that in 2021-22 draft position lined up much better with results, so the effect is not constant ([Yahoo 2022](https://ca.sports.yahoo.com/fantasy-hockey-zerog-strategy-why-you-shouldnt-draft-goalie-early-152209568.html)). His stated rule is modest: "avoid spending more draft capital on goaltenders than the others in your league." DobberHockey's Ryan Brudner reviewed six Zero G teams and concluded he is "100% convinced that ZeroG is a valuable strategy for most points leagues", while admitting the sample was self-selected success stories ([DobberHockey 2024](https://dobberhockey.com/2024/06/23/geek-of-the-week-zerog-case-studies-using-true-value-constructs)). RotoWire's 2025-26 review recommends avoiding goalies in the first three rounds, owning one strong starter and streaming the rest ([RotoWire/Eagleson 2026](https://www.rotowire.com/hockey/article/five-lessons-learned-from-2025-26-that-can-be-applied-to-next-year-111991)).

The caveats matter for us. Ian Gooding notes that Zero G was tested in 12-team drafts of 16 players, and that in deep leagues nearly every NHL goalie gets drafted, which shortens the window ([DobberHockey 2021](https://dobberhockey.com/2021/10/03/ramblings-caveats-to-zero-g-preseason-observations-on-bunting-eklund-t-johnson-oct-3/)). RotoWire's Jon Litterine says fading goalies works best with fewer teams, because in bigger leagues "any goaltender remotely worth considering is going to be drafted", and that it demands daily waiver work ([RotoWire 2025](https://www.rotowire.com/hockey/article/top-fantasy-hockey-draft-strategies-2023-96654)). Matiash explicitly advises securing a "one-two goaltending punch early on" in deeper leagues ([ESPN 2025](https://africa.espn.com/fhl/story/_/id/46216225/espn-fantasy-hockey-goalie-draft-stars-sleepers-best-picks)).

**Tiers.** A tier is a group of players whose projections are close enough to treat as interchangeable. Steve Laidlaw's rule for goalies: do not draft from a tier that is still full; wait until it starts to empty, and aim for one goalie from each of your top three tiers ([Yahoo/Laidlaw 2016](https://ca.sports.yahoo.com/news/yahoo-fantasy-hockey-secret-to-drafting-nhl-goalies-142242489.html)).

**ADP, reaching and value.** Average draft position (ADP) is where a player is typically taken. Its main use is knowing how long you can wait: RotoWire's draft guide says there is "no sense in wasting an early-round pick" on a player who will still be there later ([RotoWire](https://www.rotowire.com/hockey/advice/draft-strategy.php)). Daily Faceoff's Nick Szeman frames a bad pick as one "whose draft-day cost leaves little room for upside", typically a player priced at his ceiling after an unsustainable season ([Daily Faceoff 2026](https://www.dailyfaceoff.com/news/nick-szemans-2026-27-fantasy-hockey-adps-to-avoid)). Schedule is a tiebreaker only: prefer the player whose team plays more "light days" (days when fewer than half the league plays) when projections are close ([Daily Faceoff 2023](https://www.dailyfaceoff.com/news/fantasy-hockey-using-light-days-to-your-advantage-on-draft-day)).

## 2. Positional scarcity

**How to measure it.** Count the players rostered at each position across the league; the next player is replacement level. Yahoo's example uses 9 forwards and 5 defensemen per team; DobberHockey's example of a 12-team league with 4 D slots puts the replacement defenseman at D48, with a prior-season pace of 6 goals, 27 assists, 125 shots, 100 hits and 100 blocks ([Yahoo](https://sports.yahoo.com/fantasy-hockey-value-players-positions-based-on-league-settings-122553499.html), [DobberHockey](https://dobberhockey.com/?p=79234)).

**Defense.** The structural reason for a steep curve is power-play usage: about 32 defensemen league-wide hold a first-unit power-play spot against about 128 forwards, which "translates to a large drop off in fantasy value after 25-30 defensemen" ([Yahoo](https://sports.yahoo.com/fantasy-hockey-value-players-positions-based-on-league-settings-122553499.html)). Older data agree: in 2015-16 only 26 defensemen reached 40 points and 12 reached 50 ([DobberHockey/Laidlaw 2016](https://dobberhockey.com/?p=41343)). In ESPN default points scoring for 2022-23, nearly 30 forwards averaged 2.4 fantasy points per game or more against only seven defensemen, yet nearly 70 defensemen averaged 1.7 or more ([ESPN 2023](https://global.espn.com/fantasy/hockey/story/_/id/38373882/espn-nhl-2023-draft-how-play-win-fantasy-hockey-league-pro)). Read together: the top of the D pool is thin and valuable, and the middle is flat and replaceable. The very top has also become more valuable: five defensemen finished in Yahoo's top 25 in 2025-26 ([RotoWire/Eagleson 2026](https://www.rotowire.com/hockey/article/five-lessons-learned-from-2025-26-that-can-be-applied-to-next-year-111991)).

**Forwards and utility slots.** A utility (UTIL) slot accepts any skater and "nine times out of ten" is filled by a forward, so each UTIL slot shifts value back toward forwards ([Yahoo](https://sports.yahoo.com/fantasy-hockey-value-players-positions-based-on-league-settings-122553499.html)). Both our leagues use a single generic F slot, so the centre/wing scarcity discussed in older articles ([theScore](https://www.thescore.com/nhl/news/1353772)) does not apply.

**Goalies.** There are only 32 NHL starting jobs, and theScore ranks goalie as the scarcest position, with reliable options running out after roughly the first 15 ([theScore](https://www.thescore.com/nhl/news/1353772)). ESPN's Sean Allen uses a 65 percent "crease share" (share of a team's starts) as the threshold for a top-12 fantasy goalie ([ESPN 2025](https://www.espn.ph/fhl/story/_/id/46403603/fantasy-hockey-goalie-tandems-draft-advice-picks-points)).

**Points versus category leagues.** In points leagues, pure producers rise and scarcity acts only through replacement level, not through the need to "win" each category ([Fantrax HQ 2024](https://fantraxhq.com/?p=149074)). Peripheral stats (shots, hits, blocks) matter exactly in proportion to their weights. ESPN's worked example: a defenseman with 179 blocks, 154 hits and 18 points in 69 games came to 2.0 fantasy points per game in ESPN default scoring, the level of a good offensive defenseman (reported in ESPN's "how to win" series; I saw this figure in a search result summary and did not confirm it on the page).

## 3. Goalie roster strategy

**How many.** Matiash recommends "at least one standout operator, good for 60 or so games" plus a solid second goalie ([ESPN 2023](https://global.espn.com/fantasy/hockey/story/_/id/38373882/espn-nhl-2023-draft-how-play-win-fantasy-hockey-league-pro)). Laidlaw aims for three, one per tier ([Yahoo 2016](https://ca.sports.yahoo.com/news/yahoo-fantasy-hockey-secret-to-drafting-nhl-goalies-142242489.html)). Short benches make a third goalie expensive, so with four or fewer bench spots prioritise heavy workloads instead ([Yahoo](https://sports.yahoo.com/fantasy-hockey-value-players-positions-based-on-league-settings-122553499.html)).

**Volume versus quality.** The ideal goalie "competes often, wins a lot, and stops the largest fraction of shots" ([ESPN 2025](https://africa.espn.com/fhl/story/_/id/46216225/espn-fantasy-hockey-goalie-draft-stars-sleepers-best-picks)), but volume alone is not enough: "Just because a goaltender gets the workload, doesn't mean they get the fantasy results" ([ESPN/Allen 2025](https://www.espn.ph/fhl/story/_/id/46403603/fantasy-hockey-goalie-tandems-draft-advice-picks-points)). League-wide save percentage fell to .896 in 2025-26, a 30-year low, which raises the share of bad outings ([RotoWire/Eagleson 2026](https://www.rotowire.com/hockey/article/five-lessons-learned-from-2025-26-that-can-be-applied-to-next-year-111991)).

**Team quality.** Laidlaw argues goalie results depend heavily on the team and that last season's standings are a better guide than individual goalie forecasts ([Yahoo 2016](https://ca.sports.yahoo.com/news/yahoo-fantasy-hockey-secret-to-drafting-nhl-goalies-142242489.html)); Fantrax HQ says to pick tandem goalies from teams that win consistently ([Fantrax HQ 2024](https://fantraxhq.com/?p=149074)). The risk is a team collapse: RotoWire cites the Rangers and Predators sinking Shesterkin and Saros and suggests weighing each team's worst case ([RotoWire 2025](https://www.rotowire.com/hockey/article/top-fantasy-hockey-draft-strategies-2023-96654)).

**Tandems and handcuffs.** A tandem is two goalies sharing a team's starts; a handcuff is the backup you roster to insure your own starter. Allen says tandem partners are drafted late and are useful "especially in daily lineup leagues", calls a pair where both goalies passed 100 fantasy points the "gold standard", and suggests insuring an injury-prone starter with his partner in the late rounds ([ESPN/Allen 2025](https://www.espn.ph/fhl/story/_/id/46403603/fantasy-hockey-goalie-tandems-draft-advice-picks-points)). A handcuff behind a healthy starter who plays most games has little value (search-result summary of older articles; not individually verified).

**Negative outings.** Allen's example Monday had five goalies score between -0.8 and -8.4 ESPN points, and starters are rarely confirmed more than about 12 hours ahead, so goalie streaming is a same-day decision ([ESPN/Allen 2022](https://africa.espn.com/fhl/story/_/id/35196481/espn-nhl-how-add-drop-stream-max-your-team)).

## 4. Bench construction

- **Bench skaters earn points only when they can be started.** Off-night players (teams that play on light days) "make it less likely you have to bench an active player for another active player" ([ESPN/Allen 2022](https://africa.espn.com/fhl/story/_/id/35196481/espn-nhl-how-add-drop-stream-max-your-team)). Deep benches favour defensemen and off-night teams ([Yahoo](https://sports.yahoo.com/fantasy-hockey-value-players-positions-based-on-league-settings-122553499.html)).
- **Streaming** means adding a player for a short stretch of games and dropping him afterwards. Without limits, quantity usually beats quality outside the top 100 players; with limits it loses most of its edge, and the main sin becomes dropping a "difference-maker" ([ESPN/Allen 2022](https://africa.espn.com/fhl/story/_/id/35196481/espn-nhl-how-add-drop-stream-max-your-team)).
- **IR stashes.** Use injured-reserve slots fully, even before the season ([ESPN 2023](https://global.espn.com/fantasy/hockey/story/_/id/38373882/espn-nhl-2023-draft-how-play-win-fantasy-hockey-league-pro)). Brudner's method: estimate games missed, fill them with replacement-level production, and value the player on the combined total; the precondition is "ample IR/IR+ spots" ([DobberHockey 2023](https://dobberhockey.com/?p=79234)). An injured player held on the bench rather than IR costs a full bench slot's worth of startable games (own inference).
- **Save moves.** Matiash advises keeping transactions in reserve for goalies who emerge later in the season ([ESPN 2023](https://global.espn.com/fantasy/hockey/story/_/id/38373882/espn-nhl-2023-draft-how-play-win-fantasy-hockey-league-pro)).

## 5. Stars-and-scrubs versus balance

"Stars and scrubs" means concentrating resources in a few elite players and filling the rest with cheap ones. The best quantitative source I found is baseball: as leagues get deeper, "talent becomes scarcer", first-round picks are worth more (median $26.91 in 8-team leagues versus $33.51 in 16-team leagues), and late picks also gain value because "acquiring above-replacement players matters much more when replacement level is so low" ([FanGraphs/Vander Lugt 2026](https://fantasy.fangraphs.com/asset-valuation-pt-4-messing-with-the-draft/)). The same piece shows realised value is more concentrated than projected value because of injuries and breakouts. FanGraphs' discussion of the choice itself reaches no conclusion ([FanGraphs/Young 2023](https://fantasy.fangraphs.com/stars-and-scrubs-vs-balancing-a-roster-in-ottoneu)). In hockey, RotoWire's Litterine leans toward reliability: "you're almost always in better shape selecting the known commodity" ([RotoWire 2025](https://www.rotowire.com/hockey/article/top-fantasy-hockey-draft-strategies-2023-96654)).

My synthesis (own inference): stars-and-scrubs works when the "scrubs" can be replaced freely from waivers, which requires a shallow league and many transactions. A 14-team league with 2 adds per matchup period (7 days) has neither, so depth has to be owned, not rented. A 12-team league that starts only 12 players has a high replacement level, so there concentration in stars is right.

## 6. Rookies, breakouts and the reliability of rankings

- **Redraft:** do not overrate rookies. In 2020-21 only one top Calder candidate produced, and the rookies who succeeded were on waivers at the start ([DobberHockey/Collins 2021](https://dobberhockey.com/2021/06/21/top-10-fantasy-hockey-lessons-learned-this-year)). Fantrax HQ and RotoWire both prefer veterans with safe floors late ([Fantrax HQ 2024](https://fantraxhq.com/?p=149074), [RotoWire 2025](https://www.rotowire.com/hockey/article/top-fantasy-hockey-draft-strategies-2023-96654)).
- **Dynasty:** invest aggressively in truly elite young players; most prospects need four to five seasons, the elite do not ([RotoWire/Eagleson 2026](https://www.rotowire.com/hockey/article/five-lessons-learned-from-2025-26-that-can-be-applied-to-next-year-111991)).
- **Breakout timing:** Dobber's "breakout threshold" says average-sized forwards tend to show their level after about 200 NHL games, and very small or very large forwards and bigger defensemen after about 400; the signal is a 25 percent year-over-year jump. Dobber says about 80 percent of players fit and gives no validation study ([DobberHockey 2022](https://dobberhockey.com/2022/06/30/breakout-threshold-bt-using-career-games-played-to-determine-breakout-potential)).
- **One big season regresses:** players who build on a surprise season "are in the minority" ([DobberHockey/Collins 2021](https://dobberhockey.com/2021/06/21/top-10-fantasy-hockey-lessons-learned-this-year)).
- **Role and team drive breakouts:** Darren Raddysh finished 31st overall in Yahoo in 2025-26 after his ice time rose from 17:18 to 22:42 with an injured teammate's power-play role ([RotoWire/Eagleson 2026](https://www.rotowire.com/hockey/article/five-lessons-learned-from-2025-26-that-can-be-applied-to-next-year-111991)).
- **Ranking reliability:** skaters are more predictable than goalies. In 2025-26 Hellebuyck fell from a preseason overall rank of 62 to 148 and Oettinger from 25 to 102, while Jet Greaves rose from 193 to 86 ([RotoWire/Eagleson 2026](https://www.rotowire.com/hockey/article/five-lessons-learned-from-2025-26-that-can-be-applied-to-next-year-111991)). I found no systematic accuracy study for hockey projections.

## 7. Common drafting mistakes

- Ignoring league settings and scoring weights ([RotoWire](https://www.rotowire.com/hockey/advice/draft-strategy.php)).
- Undervaluing top defensemen through not thinking in value over replacement ([Yahoo](https://sports.yahoo.com/fantasy-hockey-value-players-positions-based-on-league-settings-122553499.html)); the mirror mistake is loading up early on non-elite defensemen ([ESPN 2023](https://global.espn.com/fantasy/hockey/story/_/id/38373882/espn-nhl-2023-draft-how-play-win-fantasy-hockey-league-pro)).
- Paying for last year's goalie statistics ([Yahoo/Laidlaw 2016](https://ca.sports.yahoo.com/news/yahoo-fantasy-hockey-secret-to-drafting-nhl-goalies-142242489.html)).
- Drafting on name, favourite team or hype; overvaluing rookies; refusing to adapt when the draft changes ([DobberHockey/Pupkov 2023](https://dobberhockey.com/2023/10/05/analytics-advantage-common-pitfalls-in-fantasy-drafts)).
- Stacking linemates as in football; coaches change lines too often ([RotoWire 2025](https://www.rotowire.com/hockey/article/top-fantasy-hockey-draft-strategies-2023-96654), [DobberHockey/Collins 2021](https://dobberhockey.com/2021/06/21/top-10-fantasy-hockey-lessons-learned-this-year)).
- Trusting per-60-minute rates over per-game numbers for players with small roles ([DobberHockey/Collins 2021](https://dobberhockey.com/2021/06/21/top-10-fantasy-hockey-lessons-learned-this-year)).
- Panicking over slow starts ([ESPN 2023](https://global.espn.com/fantasy/hockey/story/_/id/38373882/espn-nhl-2023-draft-how-play-win-fantasy-hockey-league-pro)).

## How this applies to our two leagues

Everything in this section is own inference from the league settings plus the sources above.

**ESPN, 14 teams (9 F, 5 D, 1 UTIL, 2 G, 5 bench, 2 IR; 2 adds per matchup period, which is 7 days in the regular season).**

- Starting slots league-wide: 126 F, 70 D, 14 UTIL, 28 G. If UTIL goes to forwards, 140 forwards start. Total non-IR roster spots are 22 x 14 = 308.
- Seventy starting defensemen is more than twice the roughly 32 first-unit power-play defensemen. The last starting D in this league is therefore a non-power-play defender whose value comes almost entirely from blocks (0.5), shots (0.1) and hits (0.1). A block is worth five hits and half an assist here, so block volume is the main separator in the D2-D5 range, and hits are close to noise.
- Twenty-eight starting goalie slots against 32 NHL starting jobs, with only 19 goalies playing 50 or more games last season, means startable goalie volume is nearly exhausted by starters alone. This is the "deep league" case where Zero G's own advocates say it weakens. With a three-goalie cap, up to 42 goalies can be rostered.
- Goalie scoring here (W 2, SO 2, OTL 1, SV 0.2, GA -1.5) is not ESPN's default (W 4, SO 3, GA -2 per [ESPN 2025](https://africa.espn.com/fhl/story/_/id/46216225/espn-fantasy-hockey-goalie-draft-stars-sleepers-best-picks)), so ESPN's published points-league rankings cannot be used as-is for goalies. Ignoring decisions, a start breaks even at a save percentage of 1.5 / 1.7 = .882. At last season's league average of .896 on 28 shots, saves minus goals against is about +0.65 points, and wins add roughly 1 point at a 50 percent win rate. So an average start is worth under 2 points, and a five-goals-against loss on 31 shots is about -2.3. Below-average goalies are close to zero expected value with real downside; starting a weak goalie can be worse than leaving the slot empty.
- With 2 adds per matchup period, streaming is nearly unavailable. The five bench spots should hold players good enough to start on their own off-nights, and at most one extra goalie.

**Fantrax, 12 teams, keep 3 per team (6 F, 4 D, 2 G active; up to 6 reserve but only 4 in practice because the roster cap is 16 excluding IR; 2 IR; 3 moves per week; daily lineups).**

- Starting slots league-wide: 72 F, 48 D, 24 G. Only 72 forwards start, so the replacement forward is a very good player and only top-end forwards carry much value over replacement; depth forwards are nearly interchangeable. D48 matches DobberHockey's 12-team example baseline.
- Skater weights (G 4, A 2, SOG 0.5, HIT 0.3, BLK 0.5, PIM 0.5) make a shot worth one-eighth of a goal, against one-twentieth in the ESPN league. Shot volume and hits are worth far more here, and blocks relatively less. The same player can rank very differently in the two leagues.
- Goalie weights (W 5, SO 7.5, SV 0.25, GA -1, no loss penalty) make nearly every start positive: break-even save percentage is 1 / 1.25 = .800. An average start (28 shots, .896, 50 percent win rate) is about 3.4 + 2.5 = 5.9 points, more than a goal plus a shot (4.5). Volume of starts is therefore the dominant goalie attribute, wins on good teams are a large bonus, and holding a third goalie or a tandem partner on one of the four reserve spots (the 16-player cap, not the 6-reserve maximum, is the binding limit when 12 are active) is easy to justify, though each such goalie displaces a skater from a very small bench. Zero G fits this league poorly; goalies who start a lot are premium assets.
- Because only each team's top 3 players carry over (the rest are redrafted), the breakout-threshold and elite-youth points apply to a team's three keeper slots and to the four reserve spots only as a way to find a future keeper, while the "veterans over rookies" advice applies only to who is started this season.

## Implications for the engine

Well supported by sources:

1. Value every player as projected league-specific points minus a positional replacement baseline, with the baseline derived from teams x slots for that league (UTIL counted as a forward slot).
2. Compute replacement level separately per league; never reuse one league's ranking in the other.
3. Give goalie projections wider uncertainty than skater projections, and regress goalie quality heavily toward league average; project goalie value mainly from expected starts and team strength.
4. Model a goalie start as a distribution that can be negative; before recommending a goalie start or stream, check that expected points are positive after the goals-against penalty.
5. Value an IR-eligible injured player at (his projection for games played) + (replacement-level points for games missed) when an IR slot is free; apply a penalty when IR is full and he would occupy a bench slot.
6. Count a bench skater's value in "usable games" (games on days when a lineup slot is open), not scheduled games; prefer off-night schedules as a tiebreaker.
7. When weekly moves are capped, charge each add/drop an opportunity cost and never propose dropping a player ranked well above replacement for a short-term schedule gain.
8. In redraft mode, discount rookies and one-season breakouts toward prior levels; in dynasty mode, add age and games-played (breakout threshold) to long-term value.
9. Flag role changes (ice time and first-unit power-play time) as the leading breakout signal.

Own inference, to be tested against data:

10. ESPN league: the engine should check whether a marginal goalie's expected points per start are below a replacement skater's expected points on that day, and if so prefer carrying two goalies plus skaters over three goalies.
11. ESPN league: defensemen outside the power-play tier should be ranked primarily on blocks per game; verify by back-testing D2-D5 scoring.
12. Fantrax league: rank goalies primarily by projected starts x (saves value + win probability x 5) and treat a confirmed starter's backup as a valuable reserve hold.
13. Fantrax league: the forward replacement baseline (about F72 plus reserves) should be high enough that mid-tier forwards show near-zero value over replacement; if the engine shows otherwise, the baseline is wrong.
14. A sanity test: the sum of positive value over replacement by position should be roughly proportional to the slots times the steepness of that position's curve; a position with zero total value indicates a baseline error.

## Open questions / where sources disagree

- **Goalie timing.** Zero G advocates (Groot Nibbelink, Brudner, Eagleson) versus early-goalie advocates (Wegman's net-out; Matiash for deeper leagues; RotoWire's general guide, which says to prioritise an elite netminder where goalies carry high value). Both sides accept that league depth and goalie scoring weight decide it. Nobody I read tested it on a 14-team points league with a goals-against penalty.
- **Is goalie performance random?** JFresh reports a year-to-year correlation of 0.12; other analysts hold that save percentage is repeatable given a large enough sample (seen only in a search summary of Hockey Graphs posts, not verified on the page).
- **How early for defensemen?** Laidlaw and Wegman say early and often; Doyle lists "drafting a defenseman too early" as a mistake; Matiash says only the elite ones. The difference is mostly scoring format.
- **Zero G evidence quality.** The supporting data are one author's ADP study over two seasons, with opposite results, and six self-selected case studies.
- **Projection accuracy.** No independent audit of hockey projection systems was found. The engine should measure its own error per position.
- **Stars versus depth in H2H points hockey.** No hockey-specific quantitative study was found; the section above rests on baseball evidence and inference.

## Sources

- https://sports.yahoo.com/fantasy-hockey-value-players-positions-based-on-league-settings-122553499.html
- https://ca.sports.yahoo.com/fantasy-hockey-zerog-strategy-why-you-shouldnt-draft-goalie-early-152209568.html
- https://ca.sports.yahoo.com/news/yahoo-fantasy-hockey-secret-to-drafting-nhl-goalies-142242489.html
- https://dobberhockey.com/2024/06/23/geek-of-the-week-zerog-case-studies-using-true-value-constructs
- https://dobberhockey.com/2021/10/03/ramblings-caveats-to-zero-g-preseason-observations-on-bunting-eklund-t-johnson-oct-3/
- https://dobberhockey.com/2021/10/01/ramblings-vrana-out-four-months-pettersson-hughes-come-to-terms-zero-goalie-strategy-oct-1
- https://dobberhockey.com/?p=79234 (How to Value Injured Players)
- https://dobberhockey.com/?p=41343 (Why You Should Draft Defensemen Early)
- https://dobberhockey.com/2023/10/05/analytics-advantage-common-pitfalls-in-fantasy-drafts
- https://dobberhockey.com/2021/06/21/top-10-fantasy-hockey-lessons-learned-this-year
- https://dobberhockey.com/2021/05/19/ramblings-goalie-rotations-and-their-values-canucks-defencemen-playoff-pools-more-may-19
- https://dobberhockey.com/2022/06/30/breakout-threshold-bt-using-career-games-played-to-determine-breakout-potential
- https://global.espn.com/fantasy/hockey/story/_/id/38373882/espn-nhl-2023-draft-how-play-win-fantasy-hockey-league-pro
- https://africa.espn.com/fhl/story/_/id/46216225/espn-fantasy-hockey-goalie-draft-stars-sleepers-best-picks
- https://africa.espn.com/fhl/story/_/id/35196481/espn-nhl-how-add-drop-stream-max-your-team
- https://www.espn.ph/fhl/story/_/id/46403603/fantasy-hockey-goalie-tandems-draft-advice-picks-points
- https://africa.espn.com/nhl/story/_/id/49029525/nhl-2026-playoffs-stanley-cup-final-goaltender-tandems-bussi-andersen-hurricanes
- https://www.espn.com/fantasy/hockey/story/_/id/49299647/espn-nhl-fantasy-hockey-rankings-forwards-defensemen-goalies-points-leagues (read; not relied on for claims)
- https://www.rotowire.com/hockey/article/five-lessons-learned-from-2025-26-that-can-be-applied-to-next-year-111991
- https://www.rotowire.com/hockey/article/top-fantasy-hockey-draft-strategies-2023-96654
- https://www.rotowire.com/hockey/advice/draft-strategy.php
- https://fantraxhq.com/?p=149074
- https://www.fantasypros.com/2025/10/fantasy-hockey-drafting-strategies (read; generic, not relied on)
- https://www.dailyfaceoff.com/news/fantasy-hockey-using-light-days-to-your-advantage-on-draft-day
- https://www.dailyfaceoff.com/news/nick-szemans-2026-27-fantasy-hockey-adps-to-avoid
- https://thescore.com/news/1089807
- https://www.thescore.com/nhl/news/1353772
- https://jfresh.substack.com/p/why-goaltending-is-basically-random
- https://fantasy.fangraphs.com/asset-valuation-pt-4-messing-with-the-draft/
- https://fantasy.fangraphs.com/stars-and-scrubs-vs-balancing-a-roster-in-ottoneu
- https://icehockeyman.com/2026/08/21/how-many-roster-spots-should-be-used-for-streaming-in-fantasy-hockey/ (read; low-trust, not relied on)
