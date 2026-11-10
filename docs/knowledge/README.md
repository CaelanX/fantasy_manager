# Fantasy knowledge base

Research notes on how fantasy leagues are won, written on 2026-11-10 to guide the valuation and recommendation engine. Each file states what published sources say, cites them inline, and ends with "Implications for the engine": rules the program should follow or check.

Read [09-engine-gap-analysis.md](09-engine-gap-analysis.md) first if you are about to change the engine. It compares the research with what the code does and lists the gaps in order of impact.

## Files

| File | What it answers |
|---|---|
| [01-fantasy-fundamentals.md](01-fantasy-fundamentals.md) | Formats and what each rewards; replacement level and value over replacement; scarcity; what winning a head-to-head league requires |
| [02-roster-construction-and-drafting.md](02-roster-construction-and-drafting.md) | How to split a roster across forwards, defensemen and goalies; when goalies are worth paying for; bench use |
| [03-hockey-player-value-drivers.md](03-hockey-player-value-drivers.md) | What drives a hockey player's fantasy value: ice time, power play, shots, hits and blocks, goalie workload, schedule |
| [04-projection-and-regression.md](04-projection-and-regression.md) | What predicts future production, how big a sample each stat needs, aging, and how to test a projection honestly |
| [05-in-season-management.md](05-in-season-management.md) | Daily lineups, streaming and waivers under a tight move limit, goalie management, playoff preparation |
| [06-trade-strategy.md](06-trade-strategy.md) | How to value a trade, 2-for-1 deals and the freed roster spot, acceptance and manager biases |
| [07-dynasty-and-keeper.md](07-dynasty-and-keeper.md) | Valuing players over several seasons, aging by position, prospects, draft picks, contend versus rebuild |
| [08-h2h-objectives-and-game-theory.md](08-h2h-objectives-and-game-theory.md) | What to optimise: weekly win probability, when risk helps, luck in records, playoffs, a team that is far behind |
| [09-engine-gap-analysis.md](09-engine-gap-analysis.md) | The research checked against the code: what matches, what is missing or wrong, and a suggested order of work |
| [fantrax-official-api.md](fantrax-official-api.md) | Reference for Fantrax's official API: the nine endpoints, their parameters, and what they do and do not return |

## The short version

- **Value is the margin over the alternative.** A player is worth what he adds over the best freely available replacement at his position, for the games he will actually be in your lineup ([01](01-fantasy-fundamentals.md)). Scarcity is already inside a correctly set replacement level.
- **Opportunity is the stable part of skater value.** Ice time and power-play time repeat year to year far better than shooting percentage does. Shots, hits and blocks repeat; percentages mostly do not ([03](03-hockey-player-value-drivers.md), [04](04-projection-and-regression.md)).
- **Goalies are a different asset in each of our leagues.** With ESPN's scoring a start breaks even at a save percentage of about .882, so a weak goalie is worth about nothing and can cost points. With Fantrax's scoring it breaks even at about .800, so nearly every start is positive and volume decides value ([02](02-roster-construction-and-drafting.md), [03](03-hockey-player-value-drivers.md), [05](05-in-season-management.md)). These break-even figures are the researchers' own arithmetic from the league settings.
- **A pickup is worth the lineup games it adds.** With 2 or 3 moves a week, each add has to buy several started games or a lasting upgrade ([05](05-in-season-management.md)).
- **A trade is judged on the rosters as they end up.** In a 2-for-1 the freed spot is worth only what the waiver pool offers, which is little in a 14-team league ([06](06-trade-strategy.md)).
- **The weekly goal is the chance of winning the matchup,** but taking or avoiding risk changes that chance only a little. Games played and player quality come first ([08](08-h2h-objectives-and-game-theory.md)).
- **Records are noisy.** Points scored and all-play record judge a team better than wins and losses ([01](01-fantasy-fundamentals.md), [08](08-h2h-objectives-and-game-theory.md)).
- **In dynasty, skaters hold a plateau from about 23 to 29** and the market discounts players over 30 below their production. Giving up a season is expensive because playoff matchups are close to coin flips ([07](07-dynasty-and-keeper.md)).

## How far to trust it

- Each file was written by a separate research pass that read sources through a summarising fetch tool. Quotations and exact figures should be checked against the linked page before they are relied on for a fitted constant.
- Every file labels its claims: sourced, seen only in a search summary, the researcher's own calculation, or inference. The sections on our two leagues and many of the engine implications are inference.
- Hockey-specific evidence is thin in places. Several findings come from football or baseball (playoff coin-flip rates, the effect of record luck, trade behaviour) and are marked as such.
- The researchers of files 01 to 08 did not read the code. What they say the engine does or lacks is a guess; [09](09-engine-gap-analysis.md) is where those guesses were checked.
- Some sources were paywalled or blocked (The Athletic, KeepTradeCut, Dynasty League Football, parts of Footballguys). Claims that depended on them are marked unverified or left out.

## League facts confirmed from the app

These come from `fm settings` and the provider code, and settle questions the research files left open.

| | ESPN (Monke Hockey League) | Fantrax (Dynasty League) |
|---|---|---|
| Teams and format | 14, redraft, head-to-head points, 7-day matchups | 12, dynasty, head-to-head points |
| Active slots | 9 F, 5 D, 1 UTIL, 2 G | 6 F, 4 D, 2 G (at most 12 active) |
| Bench and injured | 5 bench, 2 IR | at most 6 reserve, 2 IR |
| Roster cap | 22 excluding IR, at most 3 goalies including IR | 16 excluding IR |
| Moves | 2 adds per matchup period | 3 per week |
| Games-played caps | none | none |
| Skater scoring | G 2, A 1, HAT 1, PPP 0.5, SHP 0.5, SOG 0.1, HIT 0.1, BLK 0.5, DEF 0.1 | G 4, A 2, PPP 1, SHG 2, ENG 2, SOG 0.5, HIT 0.3, BLK 0.5, PIM 0.5, FT 3 |
| Goalie scoring | W 2, SO 2, OTL 1, SV 0.2, GA -1.5 | W 5, SO 7.5, SV 0.25, GA -1, A 3 |

- **FT is fights** (fighting majors), 3 points each (`crates/fm-providers/src/fantrax.rs`, the stat-name mapping). The research briefs wrongly described it as faceoff-related. It lifts enforcers, not centres.
- **DEF is ESPN stat 33** (`crates/fm-providers/src/espn/api.rs`), which we read as defensemen points: 0.1 extra for each point scored by a defenseman. This reading has not been checked against an ESPN rules page.
- **The Fantrax limits are separate caps.** Active 12, reserve 6 and IR 2 are each a maximum, and the total excluding IR is 16, so a full active lineup leaves 4 reserve spots.

The research files list several league facts as unknown. The program already reads most of them from the leagues (`fm outlook`, `fm playoffs`, `fm claims`, all with `--json`). Values on 2026-11-10:

| | ESPN | Fantrax |
|---|---|---|
| Playoff spots | 8 of 14 | 6 of 12 |
| Regular season left | 18 weeks | 18 weeks |
| Matchup periods | 27, playoffs start in period 25 (3 playoff periods) | playoffs start in period 25 (Fantrax Rules page) |
| Fantasy trade deadline | 2027-02-26 | none (the Rules page says "Not Used") |
| Waivers | waiver priority (no budget); a dropped player stays on waivers 24 hours; processing Sunday 08:00 | not reported by `fm claims` |
| Keepers | none (redraft) | each team keeps its top 3 players; everyone else is redrafted |

- **Fantrax is a 3-keeper league, not a full dynasty** (confirmed by the owner on 2026-11-10: "we keep 3"; Fantrax's own label for the league type is "Dynasty"). Only three players per team carry over, so future value matters for a team's best three and almost nothing for the rest. Where [07](07-dynasty-and-keeper.md) reasons about full dynasty rosters, prospects held for years or deep rebuilds, apply it to the keeper slots only.
- **More than half of each league makes the playoffs** (8 of 14 and 6 of 12). The playoff-odds figures in [08](08-h2h-objectives-and-game-theory.md) for a 1-5 team used guessed spot counts; the 8-spot case is the relevant one for ESPN.
- **Fantrax has no trade deadline,** so trades stay available through the playoffs there.
- **Fantrax lineups change daily.** The league has 194 roster periods of about one day each across the season, inside 27 weekly scoring periods (official API, `getLeagueInfo`).
- **Fantrax playoffs:** the regular season ends after period 24; 6 teams qualify; periods 25, 26 and 27 are the three rounds, ending 2027-04-11. Whether the top two seeds get a bye has not been confirmed.
- **Fantrax goalies also score penalty minutes** (0.5 each), alongside the goalie items in the table above.
- **The future draft is 10 rounds** (picks exist for 2027 to 2029, none traded yet). Ten rounds plus 3 keepers is 13 players for a 16-player roster, so the keeper count of 3 that the program uses (the `FANTRAX_KEEPERS` setting) is worth confirming with the league.
- **Fantrax waivers:** free agents are first come, first served; a dropped player stays on waivers 2 days and is then awarded by priority order. This comes from the league's rules page through the private endpoint (reported in [09](09-engine-gap-analysis.md)); the official API does not return it.

### Fantrax's official API

Fantrax publishes a small read-only API ([developer page](https://www.fantrax.com/developer)) under `https://www.fantrax.com/fxea/general/`. The full endpoint reference is in [fantrax-official-api.md](fantrax-official-api.md); those nine calls are everything the API offers. For this league these calls answer with only the league ID and no login: `getLeagueInfo` (scoring, roster limits, playoff setup, every roster and scoring period, and the full head-to-head schedule for all 27 periods), `getStandings`, `getTeamRosters`, `getDraftPicks` and `getMatchupScores`. Errors come back as HTTP 200 with an `error` field in the body.

The program does not use it today. It reads Fantrax through the site's private endpoint with a login cookie (`crates/fm-providers/src/fantrax.rs`), which is what expires and needs refreshing. Two uses worth considering: a login-free source for league settings, rosters, standings and picks when the cookie has expired, and the real remaining schedule for playoff odds, which `fm outlook` currently simulates against random opponents.
- `fm playoffs -l fantrax --json` took more than five minutes on 2026-11-10 where the ESPN call returned promptly. The cause has not been looked into.

## Using this to work on the engine

These files exist to be read back in when changing the engine. Before a change to valuation, lineups, waivers, trades or the dynasty model:

1. Read the "Implications for the engine" section of the file for that area, and the matching entries in [09](09-engine-gap-analysis.md).
2. Treat a rule marked as source-supported as a requirement to meet or to consciously reject with a reason. Treat a rule marked as inference as a hypothesis to test in the backtest or against the ledger first.
3. Check the change against real league data (`fm roster --all-teams --json`, `fm trade-eval`, the harness) and record what moved.
4. When the code or the league data shows a file is wrong, correct the file in the same change.
