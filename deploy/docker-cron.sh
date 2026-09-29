#!/usr/bin/env bash
# Scheduler for the docker-compose `cron` sidecar: sleeps until $DAILY_AT (container time zone,
# from TZ) every day, then runs deploy/daily.sh and deploy/backup.sh. No cron daemon needed.
set -u
DAILY_AT="${DAILY_AT:-07:30}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

while true; do
  now=$(date +%s)
  next=$(date -d "today $DAILY_AT" +%s)
  (( next <= now )) && next=$(date -d "tomorrow $DAILY_AT" +%s)
  echo "$(date -Is) next daily run at $(date -d "@$next" -Is)"
  sleep $(( next - now ))
  echo "$(date -Is) daily job"
  bash "$here/daily.sh" || echo "$(date -Is) daily job had failures (see data/logs/)"
  bash "$here/backup.sh" || echo "$(date -Is) backup failed"
  sleep 61   # never run twice in the same minute
done
