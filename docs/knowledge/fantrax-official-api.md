# Fantrax official API (v1.8 Beta)

Reference notes for the Fantrax REST API, taken from the documentation on Fantrax's [developer page](https://www.fantrax.com/developer) as supplied by the owner on 2026-11-10, plus what the real responses for our league showed the same day. The program does not use this API yet; see [README](README.md#fantraxs-official-api).

## General rules

- Base URL: `https://www.fantrax.com/fxea/general/`. Requests are plain HTTP and every response is JSON.
- Parameters go either in the query string or as JSON in the body of a POST. Both forms give the same response.
- **Every request must send a `User-Agent` header that names the program** (for example `User-Agent: MyDraftTool/1.0`). A request without one is rejected before it reaches the API with an HTTP 403 "Access denied" page showing error 1010.
- Fantrax describes the document as a draft and the API as beta, built out "based on users' needs". Expect fields to be added.
- Observed, not documented: an error can come back as HTTP 200 with an `error` field in the body, so check the body and not only the status code.

## Endpoints

| Endpoint | Parameters | Returns |
|---|---|---|
| `getPlayerIds` | `sport` (for us `NHL`) | The Fantrax id of every player. All other calls refer to players by these ids. Observed: 9,058 NHL entries of `{name: "Last, First", fantraxId, team, position}` |
| `getAdp` | `sport` (required: NFL, MLB, NHL, NBA, NCAAF, NCAAB, PGA, NASCAR, EPL); optional `position`, `showAllPositions` ("true"/"false"), `start`, `limit`, `order` ("Name" or "ADP", default ADP) | Average draft position for players in the sport. Observed: 972 NHL entries of `{id, name, pos, ADP}` |
| `getLeagues` | `userSecretId` (required): the Secret ID shown on the Fantrax User Profile screen | The user's leagues with each league's name and id, and the team name and id the user owns in each |
| `getLeagueInfo` | `leagueId`; optional `excludePlayerInfo` (true leaves out the large `playerInfo` element) | Team names and ids, matchups, the player pool with eligibility and status, league settings, `rosterPeriods`, and for head-to-head leagues `scoringPeriods` and `playoffs` |
| `getDraftPicks` | `leagueId` | Future and current draft picks with current and original owner |
| `getDraftResults` | `leagueId` | The league's draft results; live during a draft. An auction in progress adds `nominatedPlayerId`, `currentBid`, `currentBidTeamId` and `currentBidTimeRemainingSec` while a player is on the block |
| `getTeamRosters` | `leagueId`; optional `period` (a roster period number) | Every roster: players, their status and position, plus salary and contract data if the league uses it. Default is the current or upcoming period, which changes over once the last game of the period starts |
| `getStandings` | `leagueId` | Rank, points, win-loss-tie, games back, win percentage. Individual stats are "not yet included" |
| `getMatchupScores` | `leagueId`; optional `period` (a scoring period number) | Head-to-head scores for a scoring period, live for the current one |

## Details that matter for us

- **Periods.** Each `rosterPeriods` and `scoringPeriods` entry has `number`, `startDate` and `endDate`. They map a calendar date to the period number that `getTeamRosters` (`period=N`) and `getMatchupScores` take. Our league has 194 roster periods of about a day each and 27 weekly scoring periods.
- **Scoring format.** `scoringSystem.type` is one of `ROTISSERIE`, `POINTS_BASED`, `HEAD_TO_HEAD_POINTS_BASED`, `HEAD_TO_HEAD_ROTI_SINGLE_WIN`, `HEAD_TO_HEAD_ROTI_MULTI_WIN` or `BRACKET`. Ours is `HEAD_TO_HEAD_POINTS_BASED`.
- **Playoffs.** For head-to-head leagues `playoffs` holds `used`, `lastRegularSeasonPeriod`, `firstPlayoffPeriod`, `numPlayoffTeams` and `mergePlayoffPeriods`. Ours: used, 24, 25, 6, not merged.
- **Matchup scores in a points league.** Each matchup gives the away and home team (id and name), each team's fantasy point total, and `gamesPlayed` (games played by the team's active players in the period). It also gives a `categories` list with each scoring category's raw total (`value`, and a formatted `display`) and the fantasy points it contributed (`points`). That breakdown is left out for leagues using the "count best players" option.
- **`getLeagues` is the only call that needs a secret.** The Secret ID is a credential: keep it in `.env` or the environment, never in committed files or logs.

## Checked with real calls on 2026-11-10

- **Past scoring periods work.** `getMatchupScores` with `period=6` returned the finished week, and our matchup (191.6 to 197.55) equals the final the harness stored from the private endpoint.
- **Rosters for a named roster period work.** `getTeamRosters` with `period=42` returned that day's rosters with each player's status (`ACTIVE`, `RESERVE`, `INJURED_RESERVE`). Without `period` the call returns the upcoming lineup once the day's last game has started, so always pass the period for the wanted date.
- **Errors are HTTP 200 with an error body.** A wrong league id returned `{"error": {"onScreen": false, "code": "WARNING", "message": "Invalid 'leagueId' parameter - league ID: ... not found"}}`. A cache that stores every 2xx response would store this, so validate the body before keeping it.
- **`excludePlayerInfo=true` works** and cuts `getLeagueInfo` from about 417 KB to 49 KB.
- **No `User-Agent` gives HTTP 403** with a "Just a moment..." page, as documented.
- **Player ids match the private endpoint's.** The Fantrax id in these responses is the same string the program already uses in its player ids, so official data joins to existing players by id with no name matching.
- **Scoring-period dates need care.** A period's first day is the calendar date of `startDate`; its last day is the calendar date of `endDate` minus one, both read in the time offset the API sends. Taking the date of `endDate` directly makes each period overlap the next by a day.
- **Playoff matchups are placeholders.** In periods 25 to 27 each side is a seed number or a bye with no team id, so the schedule is only usable through period 24.

## How it compares with what the program reads today

A mapping pass on 2026-11-10 compared these responses with the data the program loads through the private endpoint. Where both carry the same fact they agreed: team ids, rosters and slots, win-loss records, all 360 draft picks, scoring weights and roster limits. The official API is therefore not a correction to today's numbers. What it adds:

- **Average draft position,** which the Fantrax league has never had in the program (the market value there runs on rostered percentage alone).
- **Structured dates and playoff settings,** in place of a regular expression over display text and a default of period 25 when the rules page cannot be read.
- **A load that survives an expired login,** for league structure, rosters, records and picks. Statistics and free agents would still be missing.
- **The real remaining schedule,** for playoff odds that are currently simulated against random opponents.

## What it does not provide

Checked against the responses for our league: no player statistics or fantasy points per player, no free-agent list beyond the pool's eligibility and status, no rostered percentage, no transactions, no waiver rules, no trade blocks, and no injury detail beyond a status. Those still need Fantrax's private endpoint, which the program already uses.
