#!/usr/bin/env bash
# Scheduler for the docker-compose `cron` sidecar (container time zone, from TZ). No cron daemon:
#   $DAILY_AT   (default 07:30)  deploy/daily.sh, then deploy/backup.sh
#   $PREGAME_AT (default 17:00)  deploy/daily.sh pregame  (fm harness pregame --league both --notify
#                                --quiet-if-unchanged; set PREGAME_AT=off to disable)
set -u
DAILY_AT="${DAILY_AT:-07:30}"
PREGAME_AT="${PREGAME_AT:-17:00}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

next_at() {  # next_at HH:MM -> epoch seconds of its next occurrence
  local now t
  now=$(date +%s)
  t=$(date -d "today $1" +%s)
  (( t <= now )) && t=$(date -d "tomorrow $1" +%s)
  echo "$t"
}

while true; do
  now=$(date +%s)
  next=$(next_at "$DAILY_AT"); job=daily
  if [[ "$PREGAME_AT" != "off" ]]; then
    p=$(next_at "$PREGAME_AT")
    (( p < next )) && { next=$p; job=pregame; }
  fi
  echo "$(date -Is) next $job run at $(date -d "@$next" -Is)"
  sleep $(( next - now ))
  if [[ $job == pregame ]]; then
    echo "$(date -Is) pre-game check"
    bash "$here/daily.sh" pregame || echo "$(date -Is) pre-game check failed (see data/logs/pregame.log)"
  else
    echo "$(date -Is) daily job"
    bash "$here/daily.sh" || echo "$(date -Is) daily job had failures (see data/logs/)"
    bash "$here/backup.sh" || echo "$(date -Is) backup failed"
  fi
  sleep 61   # never run twice in the same minute
done
