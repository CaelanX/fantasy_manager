# Daily report with Windows Task Scheduler

On a Linux server the same four steps run from a systemd timer (`deploy/daily.sh`, installed by
`deploy/install.sh`); see [hosting.md](hosting.md). A second, afternoon entry (the pre-game check
at 17:00) is described [below](#second-daily-entry-the-pre-game-check-at-1700).

The daily task runs four commands, in this order:

| Step | Command | What it does | Idempotent? |
|---|---|---|---|
| 1 | `fm auth fantrax --ping` | Keeps the Fantrax session alive (re-logs in when needed) | yes (one cheap request) |
| 2 | `fm backtest archive` | Snapshots today's projections and recommendations to `data\archive\` | yes (one file per league per day) |
| 3 | `fm harness daily` | Ingests the archive, pulls transactions, lineups, NHL results and NHL deployment (TOI / PP / goalie starts), takes the day's Daily Faceoff lines snapshot, matches your moves, grades on Mondays, refits on refit days | yes (a second run the same day changes nothing) |
| 4 | `fm report --notify` | Writes the digest and posts the summary to your webhooks | no: every run posts again |

The ping goes first so the other steps find a live Fantrax session; the report goes last so its
"Model" line and "Model health" block use today's grading. Only step 4 has a visible side effect
when repeated, so it is safe to rerun the task by hand after a failure; expect a second post.

## Before you start

1. Test the command by hand from the repo folder:

   ```powershell
   cd C:\code\fantasy_manager
   .\.venv\Scripts\fm.exe report --notify
   ```

2. Put `DISCORD_WEBHOOK_URL` and/or `SLACK_WEBHOOK_URL` in `C:\code\fantasy_manager\.env`.
   `OPENROUTER_API_KEY` is optional; it adds short explanations to each move.
3. `.env` is read from the **current working directory**, so the scheduled task has to start in
   `C:\code\fantasy_manager`. Both options below set that.

The executable is the venv's console script:
`C:\code\fantasy_manager\.venv\Scripts\fm.exe`. You don't need to activate the venv.

## Option A: PowerShell (recommended)

Run this in a normal (non-admin) PowerShell. It runs daily at 8:00 AM. If the PC was asleep or
off at that time, it runs as soon as it can.

```powershell
$repo = "C:\code\fantasy_manager"
function FmStep($fmArgs, $log) {
    New-ScheduledTaskAction -Execute "$env:ComSpec" `
        -Argument "/c `"`"$repo\.venv\Scripts\fm.exe`" $fmArgs >> `"$repo\data\$log`" 2>&1`"" `
        -WorkingDirectory $repo
}
$actions = @(
    (FmStep "auth fantrax --ping" "auth.log"),
    (FmStep "backtest archive"    "archive.log"),
    (FmStep "harness daily"       "harness.log"),
    (FmStep "report --notify"     "report.log")
)
$trigger  = New-ScheduledTaskTrigger -Daily -At 8:00AM
$settings = New-ScheduledTaskSettingsSet -StartWhenAvailable -ExecutionTimeLimit (New-TimeSpan -Minutes 20) `
    -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -RunOnlyIfNetworkAvailable
Register-ScheduledTask -TaskName "FantasyManagerDailyReport" -Action $actions -Trigger $trigger `
    -Settings $settings -Description "fm daily: fantrax ping, archive, harness, report"
```

Task Scheduler runs the actions in order, each after the previous one exits (a failed step does
not stop the next one). To change an existing task, `Unregister-ScheduledTask` it first (see
below) and register it again.

`cmd /c` is only there so stdout and stderr go to the log files in `data\`. Create the `data` folder
first if it doesn't exist (`New-Item -ItemType Directory -Force C:\code\fantasy_manager\data`).

The task runs as you, and only while you're logged in. To also run it while you're logged out,
add `-User "$env:USERDOMAIN\$env:USERNAME" -Password "<your Windows password>"` to
`Register-ScheduledTask`, or change it later in the Task Scheduler UI under *Run whether user is
logged on or not*.

## Option B: `schtasks` with a wrapper script

`schtasks` can't set a start-in folder, so use a small wrapper. Save this as
`C:\code\fantasy_manager\run_report.cmd`:

```bat
@echo off
cd /d C:\code\fantasy_manager
if not exist data mkdir data
".venv\Scripts\fm.exe" auth fantrax --ping >> data\auth.log 2>&1
".venv\Scripts\fm.exe" backtest archive >> data\archive.log 2>&1
".venv\Scripts\fm.exe" harness daily >> data\harness.log 2>&1
".venv\Scripts\fm.exe" report --notify >> data\report.log 2>&1
```

Then register it:

```bat
schtasks /Create /TN "FantasyManagerDailyReport" /SC DAILY /ST 08:00 /TR "C:\code\fantasy_manager\run_report.cmd" /F
```

## Second daily entry: the pre-game check at 17:00

Daily-lineup leagues are decided in the afternoon: Daily Faceoff confirms starting goalies through
the day, late scratches and injuries land, a call-up jumps onto a top line. The morning task can't
see any of that, so register a second task at 17:00 local:

```powershell
$repo = "C:\code\fantasy_manager"
$action = New-ScheduledTaskAction -Execute "$env:ComSpec" `
    -Argument "/c `"`"$repo\.venv\Scripts\fm.exe`" harness pregame --league both --notify --quiet-if-unchanged >> `"$repo\data\pregame.log`" 2>&1`"" `
    -WorkingDirectory $repo
$trigger  = New-ScheduledTaskTrigger -Daily -At 5:00PM
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Minutes 15) `
    -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -RunOnlyIfNetworkAvailable
Register-ScheduledTask -TaskName "FantasyManagerPregame" -Action $action -Trigger $trigger `
    -Settings $settings -Description "fm pre-game check: goalie confirmations, scratches, lineup changes"
```

No `-StartWhenAvailable` here on purpose: a check that runs after the games have started is
useless. With the wrapper-script recipe (Option B), add a `run_pregame.cmd` next to
`run_report.cmd`:

```bat
@echo off
cd /d C:\code\fantasy_manager
if not exist data mkdir data
".venv\Scripts\fm.exe" harness pregame --league both --notify --quiet-if-unchanged >> data\pregame.log 2>&1
```

```bat
schtasks /Create /TN "FantasyManagerPregame" /SC DAILY /ST 17:00 /TR "C:\code\fantasy_manager\run_pregame.cmd" /F
```

What it does (`fm harness pregame`, see `fantasy_manager/pregame.py`):

- Refreshes the fast-moving sources first: the Daily Faceoff starting-goalies page and the ESPN
  injury feed skip the cache; the line pages of tonight's teams are refetched only when the cached
  copy is older than 6 hours.
- Reloads both leagues, recomputes the lineup (this week and today: only players with a game
  today, goalies weighted by their start probability), line / role alerts and injury alerts.
- Compares with the morning: `fm harness daily` leaves a snapshot per league in
  `data\pregame\<league>-<date>.json`, and every pre-game run diffs against the latest one of the
  day, then replaces it. It reports only what changed: goalie confirmations (yours, the goalies
  your skaters face tonight, free-agent streamers), status changes and new injuries on your
  roster, your players' line / power-play moves, new line alerts, and start / sit changes against
  this morning's lineup.
- Posts a short phone-sized message (at most 1500 characters per message), e.g.
  `Pre-game 5:02 PM: Bussi CONFIRMED (CAR vs FLA). Frost moved to F2 (was F1). No lineup changes
  needed.` With `--quiet-if-unchanged` nothing is posted when nothing changed (without it you get
  `Pre-game: no changes. Lineup set.`). Run it by hand without `--notify` to just print it;
  `--json` gives the full result.
- A league that doesn't load (e.g. the Fantrax cookie expired) is reported in the same webhook
  post (`Fantrax login expired: refresh FANTRAX_COOKIE`) and the other league still runs; the task
  then exits 1. The morning `fm harness daily` sends the same notification when a league fails
  to load and webhooks are configured.

**Game time varies.** Most NHL games start at 7 PM ET, some at 7:30, 8, 10 or 10:30 PM, and
weekend matinees at 1 PM. 17:00 ET covers the 7 PM starts, about two hours before puck drop, after
most goalie confirmations. In another time zone, schedule the same moment in your local time
(14:00 Pacific, 16:00 Central). A second run later in the evening does no harm: each run only
reports what changed since the previous one. `FM_PREGAME_HOUR` in `.env` documents the hour you
picked; the scheduler sets the actual time. On a Linux server, `deploy/install.sh` installs
`fantasy-pregame.timer` (17:00, server time zone); the docker sidecar uses `PREGAME_AT`
(default `17:00`, `off` disables it).

## Managing the task

```powershell
Start-ScheduledTask -TaskName "FantasyManagerDailyReport"              # run now
Get-ScheduledTaskInfo -TaskName "FantasyManagerDailyReport"            # LastRunTime / LastTaskResult (0 = OK)
Get-Content C:\code\fantasy_manager\data\report.log -Tail 40           # recent output
Unregister-ScheduledTask -TaskName "FantasyManagerDailyReport" -Confirm:$false   # remove
```

The `schtasks` equivalents are `schtasks /Run /TN ...`, `schtasks /Query /TN ... /V /FO LIST`
and `schtasks /Delete /TN ... /F`.

## Notes

- **Cookies expire.** ESPN `espn_s2`/`SWID` eventually stop working. When that happens the log
  shows an auth error and no digest is sent. Refresh the cookies in `.env`. For Fantrax, set
  `FANTRAX_USERNAME` / `FANTRAX_PASSWORD` (see "Fantrax setup" in the README) and fm logs in again
  by itself when the session expires; otherwise refresh `FANTRAX_COOKIE` the same way.
- **Fantrax keepalive.** Both recipes start with `fm auth fantrax --ping`, logging to
  `data\auth.log`. It makes one uncached, authenticated request (`getFantasyLeagueInfo`), which
  keeps the session in regular use, re-logs in if Fantrax says the session is gone (when
  credentials are set), and records the result for `fm auth fantrax --status`. A failed ping exits
  non-zero, so the task's LastTaskResult shows it. Skip it if you don't use Fantrax.
- **Free LLM models are rate-limited.** If OpenRouter returns 429 or errors, the report is still
  written and sent, just without the narrative sentences. `FM_LLM_FALLBACKS` (comma separated)
  lists extra models for OpenRouter to try.
- **Webhook failures don't stop the report.** Each target's result (`discord: sent 1 message(s)`
  or `discord: error HTTP 404 ...`) goes to the log. Webhook URLs are never printed.
- **Time of day.** NHL injury and lineup news settles by late morning ET, and goalie
  confirmations come through the afternoon: that is what the pre-game check at 17:00 (above) is
  for. Don't add a second trigger to the morning task instead; `fm report --notify` would post the
  whole digest again.
- **Both leagues.** Register a second task, or put two lines in the wrapper script, with
  `--league espn` and `--league fantrax`.
- **Projection archive.** Both recipes run `fm backtest archive` after the ping, logging to
  `data\archive.log`. It saves the day's
  ESPN and Fantrax projections and recommendations to `data\archive\`, one file per league per
  day, so running it more than once a day is harmless. Cold it takes about 40s; with a warm cache
  it takes about 15s. `fm backtest grade` scores the archive after the season. See
  `docs/backtesting.md`.
- **Harness.** Both recipes run `fm harness daily` right after the archive, logging to
  `data\harness.log`. It ingests the archive into `data\harness.db`, pulls league transactions,
  lineups and yesterday's NHL results, and matches recommendations to your moves. It is
  idempotent (a second run the same day changes nothing) and skips the archive step when today's
  files already exist. Run it every day: the ESPN and Fantrax activity feeds only keep the most
  recent moves. The dashboard's Health tab shows the last daily run and any capture warnings.
  See `docs/harness.md`.
- **Betting odds.** With `ODDS_API_KEY` in `.env`, `fm harness daily` also fetches NHL moneylines and totals from The Odds API once a day (2 of the free tier's 500 monthly credits) and archives them with implied team totals to `data\archive\odds-YYYY-MM-DD.json`; without a key the step is skipped. `fm harness odds` shows the slate and the remaining quota.
