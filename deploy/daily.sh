#!/usr/bin/env bash
# Daily job: the same four steps as the Windows task in docs/scheduling.md, in the same order.
#   1. fm auth fantrax --ping   keep the Fantrax session alive (re-login when needed)
#   2. fm backtest archive      snapshot today's projections and recommendations
#   3. fm harness daily         ingest, match your moves, grade (Mondays), refit (refit days)
#   4. fm report --notify       write the digest and post it to your webhooks
# Every step is best-effort: a failure is logged and the next step still runs. The script exits 1
# if any step failed, so `systemctl status fantasy-daily` / `systemctl --failed` show it.
# Output goes to data/logs/<step>.log (rotated at 5 MB) and a one-line summary per step to stdout
# (the journal under systemd). Run it by hand with: sudo -u fantasy /opt/fantasy_manager/deploy/daily.sh
set -u
umask 077

APP_DIR="${APP_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$APP_DIR" || { echo "cannot cd to $APP_DIR" >&2; exit 1; }

if [[ -n "${FM_BIN:-}" ]]; then
  FM="$FM_BIN"
elif [[ -x "$APP_DIR/.venv/bin/fm" ]]; then
  FM="$APP_DIR/.venv/bin/fm"
else
  FM="$(command -v fm || true)"
fi
if [[ -z "$FM" ]]; then
  echo "fm not found (expected $APP_DIR/.venv/bin/fm); run deploy/install.sh first" >&2
  exit 1
fi

DATA_DIR="${FM_DATA_DIR:-$APP_DIR/data}"
LOG_DIR="$DATA_DIR/logs"
mkdir -p "$LOG_DIR"
STEP_TIMEOUT="${STEP_TIMEOUT:-20m}"

rotate() {  # keep one previous file once a log passes 5 MB
  local f="$1"
  if [[ -f "$f" ]] && (( $(stat -c %s "$f" 2>/dev/null || echo 0) > 5242880 )); then
    mv -f "$f" "$f.1"
  fi
}

failed=0
step() {  # step <log-name> <fm args...>
  local name="$1"; shift
  local log="$LOG_DIR/$name.log" start rc
  rotate "$log"
  start=$(date +%s)
  printf '\n===== %s  fm %s\n' "$(date -Is)" "$*" >> "$log"
  timeout "$STEP_TIMEOUT" "$FM" "$@" >> "$log" 2>&1
  rc=$?
  if (( rc == 0 )); then
    echo "ok     fm $*  ($(( $(date +%s) - start ))s)"
  else
    failed=1
    echo "FAILED fm $*  (exit $rc after $(( $(date +%s) - start ))s; see $log)"
    printf '===== exit %s\n' "$rc" >> "$log"
  fi
}

step auth    auth fantrax --ping
step archive backtest archive
step harness harness daily
step report  report --notify

exit "$failed"
