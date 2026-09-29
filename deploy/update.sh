#!/usr/bin/env bash
# Update Fantasy Manager in place: pull the code, reinstall the package, refresh the systemd units
# and restart the dashboard. Your .env and data/ are not touched.
#
#   sudo bash /opt/fantasy_manager/deploy/update.sh
set -euo pipefail

APP_DIR="${APP_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
UNITS=(fantasy-web.service fantasy-daily.service fantasy-daily.timer fantasy-backup.service fantasy-backup.timer)
[[ $EUID -eq 0 ]] || { echo "run as root: sudo bash $0" >&2; exit 1; }
cd "$APP_DIR"

before="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
if git remote get-url origin >/dev/null 2>&1; then
  echo "==> git pull"
  git pull --ff-only
else
  echo "==> no git remote: copy the new code into $APP_DIR first (see docs/hosting.md), then rerun"
fi
after="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
find "$APP_DIR" -path "$APP_DIR/data" -prune -o -path "$APP_DIR/.env" -prune -o -exec chown root:root {} +
# A tree copied from Windows may have CRLF line endings, which bash and systemd reject.
for f in "$APP_DIR"/deploy/*; do
  if grep -q $'\r' "$f"; then sed -i 's/\r$//' "$f"; echo "fixed line endings: $f"; fi
done

echo "==> pip install -e .[web]"
"$APP_DIR/.venv/bin/python" -m pip install -q --upgrade pip
"$APP_DIR/.venv/bin/python" -m pip install -q -e "$APP_DIR[web]"
"$APP_DIR/.venv/bin/python" -m compileall -q "$APP_DIR/fantasy_manager" >/dev/null || true

echo "==> systemd units"
changed=0
for unit in "${UNITS[@]}"; do
  tmp="$(mktemp)"
  sed "s#/opt/fantasy_manager#${APP_DIR}#g" "$APP_DIR/deploy/$unit" > "$tmp"
  if ! cmp -s "$tmp" "/etc/systemd/system/$unit"; then
    install -m 644 "$tmp" "/etc/systemd/system/$unit"; echo "updated $unit"; changed=1
  fi
  rm -f "$tmp"
done
(( changed )) && systemctl daemon-reload
systemctl enable fantasy-web.service fantasy-daily.timer fantasy-backup.timer >/dev/null

echo "==> restarting fantasy-web"
systemctl restart fantasy-web.service
sleep 2
if curl -fsS --max-time 5 http://127.0.0.1:8765/healthz >/dev/null; then
  echo "dashboard is up ($before -> $after)"
else
  echo "dashboard did not answer; check: journalctl -u fantasy-web -n 50" >&2
  exit 1
fi
