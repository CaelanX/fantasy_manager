# Daily report with Windows Task Scheduler

The daily task runs four commands, in this order:

| Step | Command | What it does | Idempotent? |
|---|---|---|---|
| 1 | `fm auth fantrax --ping` | Keeps the Fantrax session alive (re-logs in when needed) | yes (one cheap request) |
| 2 | `fm backtest archive` | Snapshots today's projections and recommendations to `data\archive\` | yes (one file per league per day) |
| 3 | `fm harness daily` | Ingests the archive, pulls transactions, lineups and NHL results, matches your moves, grades on Mondays, refits on refit days | yes (a second run the same day changes nothing) |
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
- **Time of day.** NHL injury and lineup news settles by late morning ET, so a second run before
  puck drop is useful on game days. To add one, append another trigger:
  `$trigger = @((New-ScheduledTaskTrigger -Daily -At 8:00AM), (New-ScheduledTaskTrigger -Daily -At 4:00PM))`.
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
