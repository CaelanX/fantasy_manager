# fantasy-manager

A command-line assistant for fantasy hockey. It connects to your ESPN league and your Fantrax (dynasty) league, pulls NHL stats, schedules, injuries and news, values every rostered player and free agent on a per-game basis using your league's own scoring, and recommends moves with the numbers behind them: lineup changes, waiver pickups, trades, sell-high/buy-low flags and injury alerts. An optional LLM (OpenRouter) adds short explanations and answers questions; a daily digest can be posted to Discord/Slack.

Points, categories and roto leagues are supported. In categories/roto leagues the "FPG" columns are per-game z-score value summed over your categories (fitted on the league's rostered players), not fantasy points.

## How players are valued

1. **Baseline.** The last three NHL seasons weighted 5/4/3 by games played, shrunk toward the positional mean with k = 8 games (forwards), 14 (defense) or 120 (goalies), times a fitted year-over-year age factor (young forwards improve, 30+ decline). When the league has a projection (ESPN or Fantrax), the baseline is 0.6 × projection + 0.4 × that history for players with 40+ NHL games over those seasons, the projection alone for rookies, and a linear mix in between. These constants were fitted on 10 seasons of NHL history (`fm backtest`, see [docs/backtesting.md](docs/backtesting.md)) and ship in `fantasy_manager/valuation/fitted_params.json`.
2. **Rates.** Current-season per-game stats are shrunk toward the baseline: `rate = (GP*current + k*baseline)/(GP+k)`, with k=25 for skaters and 40 for goalies. With 0 games played the value is just the baseline, so the tool works preseason.
3. **Recent form.** Season 0.85, last 30 days 0.15, last 15 and last 7 days 0: in the backtest, short windows added noise rather than signal at every in-season checkpoint. A split with fewer games than expected gets a proportionally smaller weight, and the difference goes back to the season weight.
4. **Scoring.** FPG = sum of (league point value × per-game rate).
5. **Availability.** Healthy 1.0, day-to-day 0.75, out 0 this week / 0.6 for the season, IR 0 / 0.4, LTIR 0 / 0.1, suspended 0 / 0.5.
6. **Replacement and VORP.** Replacement level per slot is the mean of the top 3 free-agent FPG at that slot. A player's VORP is his FPG minus the replacement level at his scarcest eligible slot.

Waiver picks need at least +0.3 FPG over your weakest same-position player who is not in an IR slot. They are ranked by gain × confidence, where confidence = GP/(GP+k) (minimum 0.25).

## Setup

Requires Python 3.12. On Windows:

```bash
python -m venv .venv
.venv/Scripts/python -m pip install -e .[dev]        # add ,web for the dashboard: -e .[dev,web]
cp .env.example .env      # then edit .env
.venv/Scripts/fm --help
```

### ESPN credentials

- `ESPN_LEAGUE_ID`: the `leagueId=` number in your league URL (`https://fantasy.espn.com/hockey/league?leagueId=123456`).
- `ESPN_YEAR`: ESPN labels a season by the year it ends, so 2026-27 is `2027`. Leave it blank to auto-detect (September onward counts as the next season).
- `ESPN_S2` and `ESPN_SWID`: private leagues need both cookies. Log in at fantasy.espn.com in a desktop browser and open DevTools (F12). In Chrome/Edge, go to Application > Storage > Cookies > `https://fantasy.espn.com`; in Firefox, go to Storage > Cookies. Copy the values of `espn_s2` (a long string) and `SWID` (keep the curly braces, e.g. `{ABCD-...}`). Treat them like passwords. They last for months, but you'll need to copy them again after logging out.
- `ESPN_TEAM` (optional): your team id or part of its name. If you leave it blank, your team is found by matching `SWID` to the team owners. `fm settings` lists team ids.

Other variables (Fantrax, OpenRouter, webhooks) are documented in `.env.example` and in the table under [Commands](#commands).

### Caching and offline mode

Every HTTP request goes through a SQLite cache at `$FM_DATA_DIR/cache.db` (default `./data`). ESPN responses are reused for 15 minutes. Cookies are never written to the cache. With `FM_OFFLINE=1`, nothing goes to the network and uncached requests fail with a clear error.

## Commands

Every command takes `--league espn|fantrax` (default `espn`) and `--json`, either globally (`fm --league fantrax --json roster`) or after the command. `--json` output always includes an `errors` list (a recommender or data source that failed) and a `meta` block with data-freshness notes and warnings. Commands that value players accept `--deep` to also fetch NHL game logs (recent form for Fantrax, which has no L7/L15/L30 splits). Run `fm <command> --help` for every option.

| Command | What it does | Example |
|---|---|---|
| `settings` | League name, scoring (point values or categories), roster shape, teams, and for Fantrax the rules found in league info | `fm --league fantrax settings` |
| `roster` | Your roster (or `--all-teams`) with GP, FPG season/week, games in the next 7 days, projected week points, VORP, and dynasty value in dynasty leagues | `fm roster --all-teams` |
| `waivers` | Free agents that beat your weakest same-position player, with the drop and reasons | `fm waivers --horizon week -n 15` |
| `lineup` | Optimal starting lineup for the week (or season) and start/sit changes | `fm lineup` |
| `injuries` | Injured players on your roster, status changes since the last run, IR moves and activations | `fm injuries --no-record` |
| `trades` | 1-for-1 and 2-for-1 proposals that raise your lineup by > 0.5 FPG, pass a ±12% fairness band (VORP, or dynasty value in dynasty leagues) and don't hurt the other team much; uses Fantrax trade-block wants | `fm trades --per-team 2 -n 5` |
| `flags` | Sell-high (your hot players with a shooting%/SV% spike) and buy-low (cold players whose shot rate held up) | `fm flags` |
| `advise` | Everything above in one ranked list, grouped by kind (scores rescaled 0-10 per kind); `--explain` adds LLM explanations | `fm advise --explain` |
| `news` | RotoWire/ESPN news matched to your roster and free agents (`--all`: every league player), with tags and age | `fm news --all -n 50` |
| `ask` | Ask a question; the LLM answers from your league data, recommendations and news only | `fm ask "should I trade Hughes for Makar?"` |
| `report` | Daily digest `digest-YYYY-MM-DD.md` + `.html` in `--out` (default `$FM_DATA_DIR/reports`); `--notify` posts the summary to your webhooks, `--explain` adds LLM explanations | `fm report --notify` |
| `notify` | Send a test message to the configured webhooks | `fm notify "hello from fm"` |
| `sync` | Refresh data (`--refresh` clears the cache) and review/confirm player matches (`--review`, `--confirm espn:123=8478402`) | `fm sync --review` |
| `mode` | Show or set the dynasty mode (`contend`, `balanced`, `rebuild`) used by every command and the dashboard; saved in `$FM_DATA_DIR/prefs.json`, `--clear` falls back to `FANTRAX_MODE`. `roster`, `waivers`, `trades`, `advise` and `report` also take `--mode` for a single run (not saved) | `fm mode rebuild` |
| `web` | Phone-friendly dashboard (needs `pip install -e .[web]`). Without `FM_WEB_PASSWORD` it answers on this PC only, and `--host 0.0.0.0` is refused unless `FM_WEB_ALLOW_INSECURE=1`; with it, every page asks for that password first (session cookie, 5 failed tries per 15 min per IP). To run it on a server, see [Hosting](#hosting). Dynasty leagues get a **Mode: Contend / Balanced / Rebuild** toggle in the header that saves the mode and recalculates. The **Health** tab (`/health`, JSON at `/api/health.json`) shows how the model is doing: accuracy and hit-rate charts with trust labels, a followed/ignored scorecard, the params changelog with **Rollback**, a refit dry run and data-capture warnings. Every page's footer shows the valuation params in use (packaged fit, plus the harness version when one is active) | `fm web --port 8765` |
| `backtest` | `data`, `run`, `fit`, `archive`, `grade`, `report`: score the projection model on NHL history, fit its constants, and archive the ESPN/Fantrax projections daily (`fm backtest archive`) so they can be graded at season end. See [docs/backtesting.md](docs/backtesting.md) | `fm backtest archive` |
| `harness` | The evaluation harness: `daily` (capture your moves and NHL results, grade on Mondays, refit on refit days), `status`, `grade`, `ledger`, `params`, `refit`, `rollback`, `auto-apply on/off`, `rebuild`. Grades every recommendation and projection against what happened and corrects a few valuation parameters within strict limits. See [docs/harness.md](docs/harness.md) | `fm harness status` |

Environment variables each command needs (all live in `.env`, see `.env.example`):

| Needed for | Variables |
|---|---|
| Any `--league espn` command | `ESPN_LEAGUE_ID`; `ESPN_S2` + `ESPN_SWID` for private leagues; optional `ESPN_YEAR`, `ESPN_TEAM` |
| Any `--league fantrax` command | `FANTRAX_LEAGUE_ID`, `FANTRAX_COOKIE` or `FANTRAX_COOKIE_FILE`; optional `FANTRAX_POINTS` (fallback when the Rules page cannot be read), `FANTRAX_TEAM`, `FANTRAX_DYNASTY`, `FANTRAX_KEEPER_HORIZON_YEARS`, `FANTRAX_MODE` |
| `ask` (required), `advise --explain`, `report --explain` | `OPENROUTER_API_KEY`; optional `FM_LLM_MODEL` (default `openrouter/free`), `FM_LLM_FALLBACKS` |
| `report --notify`, `notify` | `DISCORD_WEBHOOK_URL` and/or `SLACK_WEBHOOK_URL` |
| `web` on a network or server | `FM_WEB_PASSWORD` (required off-localhost); optional `FM_WEB_SECRET`, `FM_WEB_SESSION_DAYS`, `FM_WEB_ALLOW_INSECURE` |
| Everything | optional `FM_DATA_DIR` (default `./data`), `FM_OFFLINE=1` (cache only, no network) |

Without `OPENROUTER_API_KEY`, `advise --explain` and `report --explain` still work and print a one-line hint; `ask` exits with an error. `notify` and `report --notify` exit with code 1 when no webhook is configured or a post fails, so scheduled runs show up as failed.

To run the report every morning with Windows Task Scheduler, see [docs/scheduling.md](docs/scheduling.md). The daily task runs `fm auth fantrax --ping`, `fm backtest archive`, `fm harness daily` and `fm report --notify`, in that order: the archive snapshots both leagues' projections and recommendations to `data/archive/`, and the harness grades them (see [docs/harness.md](docs/harness.md)).

**Params provenance.** Valuation constants ship in `valuation/fitted_params.json`; the harness may layer a fitted version on top (`<FM_DATA_DIR>/harness/params/`). `fm settings`, `fm harness params` and the dashboard footer show which is in use (e.g. `fit 2026-09-28 (espn) + harness v0003 (2026-11-17)`); `fm harness rollback` or the Health tab undoes a version, and `FM_PARAMS_OVERRIDE=0` ignores harness versions.

## Hosting

To reach the dashboard from your phone anywhere and have the daily job run without your PC, host it on a small Ubuntu server (about US$5-8 a month plus a domain). [docs/hosting.md](docs/hosting.md) walks through it step by step: pick a VPS, point a domain at it, run `deploy/install.sh` (Caddy with automatic HTTPS in front of the dashboard, systemd timers for the 07:30 daily job and a nightly backup), put your `.env` on the server and set `FM_WEB_PASSWORD`. It also covers updates, backups, refreshing the Fantrax cookie remotely, monitoring, a Docker alternative (`Dockerfile`, `docker-compose.yml`) and a Tailscale option that exposes nothing to the internet.

## Development

```bash
.venv/Scripts/python -m pytest -q
```

The tests use no network. ESPN parsing is tested against `tests/fixtures/espn/player_stats_sample.json`, a hand-built fixture shaped like `espn_api` `Player.stats`.

Layout: `config.py` (settings), `cache.py` (HTTP cache and espn_api hook), `models.py`, `providers/` (ESPN, Fantrax, NHL, injuries, news, enrichment), `matching/`, `scoring/` (points, categories, roto), `valuation/` (blend, adjust, schedule, replacement, valuate, dynasty), `recommend/` (lineup, waivers, injuries, trades, flags, advise), `llm/` (OpenRouter client, narration, ask), `report/` (digest, news matching, webhooks), `web/` (dashboard), `cli.py`.

## Status

Milestones 1-5 (ESPN and Fantrax providers, NHL data and matching, lineup and injuries, dynasty values, trades, flags, categories/roto, news, LLM narration and `ask`, digest and webhooks) are in place. The web dashboard (`fm web`) is milestone 6.

The tool only recommends moves. You make them on ESPN or Fantrax yourself.

## Fantrax setup

Fantrax has no public API for private leagues, so the tool uses the same JSON endpoint as the Fantrax website (`https://www.fantrax.com/fxpa/req`) with a logged-in session.

1. **League id.** Open your league on fantrax.com. The id is the part of the URL after `/league/`, for example `fantrax.com/fantasy/league/abc123xyz/home`. Put it in `FANTRAX_LEAGUE_ID`.
2. **Login (recommended).** Put your Fantrax email (or user id) and password in `FANTRAX_USERNAME` and `FANTRAX_PASSWORD` in `.env`. The tool logs in the way the Fantrax web app does (a `login` message to the same endpoint), saves the session cookies to `$FM_DATA_DIR/fantrax_session.json` (owner-only permissions where the OS supports it) and uses that saved session first. When Fantrax later answers "not logged in", it logs in again once, retries the request and shows "Fantrax session refreshed by login" in the footer. Logins are limited to one attempt per 10 minutes, so a broken login can't hammer Fantrax. The password is stored only in your local `.env`. It is never printed, logged, cached or shown by `fm settings` (the settings are secret fields excluded from any output). `fm auth fantrax --login` logs in now, `--status` shows the saved session's age and which credentials are set (present/absent only), `--ping` makes one cheap request to check the session (the daily task in `docs/scheduling.md` runs it), and `--logout` deletes the saved session. Automated login can't pass two-factor authentication, and Fantrax's login page uses reCAPTCHA, which may reject a scripted login (`BAD_INTERACTION`). If the login fails, the error says why; use the cookie (step 3) instead.
3. **Cookie (alternative).** While logged in, open DevTools (F12) and go to the **Network** tab. Reload the page and click any request named `req?leagueId=...`. Under **Request Headers**, copy the whole value of `Cookie` into `FANTRAX_COOKIE`. The important cookie is `FX_RM` (the long-lived "remember me" login), but copying the whole header is simplest. You can also save the cookies to a file and point `FANTRAX_COOKIE_FILE` at it. The file can hold the header string, a Netscape `cookies.txt` export from a browser extension, or a JSON cookie export. Treat the cookie like a password. It is never logged and never written to the cache.
4. **Your team (optional).** Fantrax reports which team belongs to the logged-in account, so this is usually automatic. If it isn't, set `FANTRAX_TEAM` to your team id or part of its name. The error message lists every team.
5. **Point values.** The tool reads your league's scoring table from the Fantrax Rules page (`getLeagueRulesOld`), including a separate goalie group (for example goalie goals worth 20 instead of 4), and uses it automatically. `FANTRAX_POINTS` is only the fallback when that page can't be read. It takes `STAT=points` pairs, and a `goalie.` prefix sets a goalie-only value: `G=4,A=2,SOG=0.5,PPP=1,SHG=2,HIT=0.3,BLK=0.5,PIM=0.5,ENG=2,FT=3,W=5,SV=0.25,GA=-1,SO=7.5,goalie.G=20,goalie.A=3` (`ENG` is empty-net goals and `FT` is fights). `fm --league fantrax settings` checks the point values in use against Fantrax's own FPts and prints the mean absolute error. When the error is above 0.1 FP/G, run `fm --league fantrax settings --fit-points`. It fits the point values to Fantrax's FPts by least squares (skaters and goalies separately) and prints a ready-to-paste `FANTRAX_POINTS=` line. If neither the Rules page nor `FANTRAX_POINTS` is available, the tool falls back to Fantrax's FP/G directly. That works in season, but prior-season stats and projections can't be valued.
6. **Dynasty.** The Rules page is also the source for roster limits (total, active per position, reserve, IR and minors slots), the keeper league type, draft-pick trading (future years and rounds), lineup-change timing, playoffs and waivers. `fm --league fantrax settings` prints these along with your draft picks by year. A "Dynasty" keeper league type turns dynasty valuation on. You can also force it with `FANTRAX_DYNASTY=true`. `FANTRAX_KEEPER_HORIZON_YEARS` sets the horizon (default 3). Dynasty value projects each player's production along a position-specific production-by-age curve (fitted on NHL history: forwards peak at 25, a 19-year-old forward is at 76% of peak and a 31-year-old at 79%; defense peaks at 23 and declines more slowly; goalies hand-set, peak 27-32), so a 19-year-old's value grows and a 33-year-old's declines, plus a terminal term for the seasons beyond the horizon. Young players (23 and under) drafted in the first two rounds get a pedigree boost on their future years (from NHL player pages, cached 30 days), fading out by age 25 or 200 NHL games. The model value is blended 65/35 with a market value implied by Fantrax's league-wide % rostered. The dynasty mode sets the priority: `contend` (this season weighs most, and trades may not cost your current lineup more than 0.3 FPG), `balanced` (the default: a plain 0.8/year discount), or `rebuild` (later seasons weigh most). Switch it from the dashboard header toggle or with `fm mode <mode>`: the choice is saved in `$FM_DATA_DIR/prefs.json` and wins over `FANTRAX_MODE` in `.env`, which wins over the `balanced` default (`fm mode` with no argument, `fm settings` and every footer show the mode and where it came from). `--mode` on a single command overrides all of them for that run only. On the dashboard a mode change reuses the already-loaded league, NHL and news data and reruns only dynasty valuation and the recommendations, so it takes a few seconds (about 3 s against about 16 s for a full load of a 12-team Fantrax league) rather than a full reload. Waiver drops never cut a protected young player unless the pickup is worth 1.5x as much: 23 or younger and either a first-round pick, 80%+ rostered league-wide, or under 82 NHL games and 60%+ rostered. Veterans are compared on dynasty value alone (the pickup must be worth 1.15x). Ages come from NHL birth dates, else Fantrax's Age column.

Session order: the saved session file (unless it is known to be expired) > `FANTRAX_COOKIE` / `FANTRAX_COOKIE_FILE` > a fresh login with `FANTRAX_USERNAME` / `FANTRAX_PASSWORD`. If Fantrax rejects the session and no username/password is set, the tool tells you to refresh `FANTRAX_COOKIE` (step 3).

Fantrax responses are cached as JSON under `$FM_DATA_DIR/fantrax_cache` for 15 minutes, and `FM_OFFLINE=1` reads only from that cache.

**Which stats are used.** By default, Fantrax's roster and player tables show "Projected - Per Game" for the current scoring period. In that view, GP is the number of games in the period (1 to 4), not season games played. So the tool asks for two views explicitly. The current season's year-to-date view becomes the `season` line, where GP is real games played (0 before opening night). Fantrax's full-season projection becomes the `projected` line. The per-period view is never used as season stats.

**Free agents.** The free-agent pool comes from an undocumented Fantrax call (`getPlayerStats`). Fantrax's default player group is skaters only, and the "All" group has no stat columns. So skaters (`HOCKEY_SKATING`) and goalies (`POS_<goalie position id>`) are fetched separately, each for the projection and the season-to-date views. Each player's `pct_owned` comes from the Ros (% rostered) column. If the call fails, you'll see a warning, and replacement levels fall back to rostered players.
