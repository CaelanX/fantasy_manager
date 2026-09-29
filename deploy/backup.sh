#!/usr/bin/env bash
# Back up the data directory (harness ledger, archives, prefs, sessions, reports) to
# $BACKUP_DIR/fantasy-data-YYYYmmdd-HHMMSS.tar.gz and keep the newest $KEEP archives.
# SQLite files are copied with SQLite's online backup API, so a running dashboard or daily job
# can't leave a half-written copy. The HTTP caches (cache.db, fantrax_cache/) are skipped: they
# refill themselves. Backups contain your Fantrax session and dashboard key: they stay 0600.
#
#   sudo -u fantasy /opt/fantasy_manager/deploy/backup.sh          # by hand
#   restore: sudo systemctl stop fantasy-web && sudo -u fantasy tar -xzf <archive> -C /opt/fantasy_manager
set -euo pipefail
umask 077

APP_DIR="${APP_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
DATA_DIR="${FM_DATA_DIR:-$APP_DIR/data}"
case "$DATA_DIR" in /*) ;; *) DATA_DIR="$APP_DIR/${DATA_DIR#./}" ;; esac
BACKUP_DIR="${BACKUP_DIR:-/var/backups/fantasy_manager}"
KEEP="${KEEP:-14}"
if [[ -x "$APP_DIR/.venv/bin/python" ]]; then PY="$APP_DIR/.venv/bin/python"; else PY="$(command -v python3)"; fi

[[ -d "$DATA_DIR" ]] || { echo "no data directory at $DATA_DIR" >&2; exit 1; }
mkdir -p "$BACKUP_DIR"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/data"

# 1. Copy everything except the caches and SQLite files.
tar -C "$DATA_DIR" \
    --exclude='fantrax_cache' \
    --exclude='*.db' --exclude='*.db-wal' --exclude='*.db-shm' --exclude='*.db-journal' \
    -cf - . | tar -C "$stage/data" -xf -

# 2. Snapshot each SQLite database (except the HTTP cache) consistently.
while IFS= read -r -d '' db; do
  rel="${db#"$DATA_DIR"/}"
  [[ "$rel" == "cache.db" ]] && continue
  mkdir -p "$stage/data/$(dirname "$rel")"
  "$PY" - "$db" "$stage/data/$rel" <<'PY'
import sqlite3, sys
src = sqlite3.connect(sys.argv[1], timeout=60)
dst = sqlite3.connect(sys.argv[2])
with dst:
    src.backup(dst)
dst.close()
src.close()
PY
done < <(find "$DATA_DIR" -name '*.db' -type f -print0)

# 3. Archive and prune.
out="$BACKUP_DIR/fantasy-data-$(date +%Y%m%d-%H%M%S).tar.gz"
tar -C "$stage" -czf "$out.partial" data
mv -f "$out.partial" "$out"
chmod 600 "$out"
echo "backup written: $out ($(du -h "$out" | cut -f1))"

mapfile -t old < <(ls -1t "$BACKUP_DIR"/fantasy-data-*.tar.gz 2>/dev/null | tail -n +"$((KEEP + 1))")
for f in "${old[@]}"; do
  rm -f -- "$f" && echo "pruned: $f"
done
