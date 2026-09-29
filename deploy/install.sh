#!/usr/bin/env bash
# Install (or re-install) Fantasy Manager on Ubuntu 24.04: dashboard behind Caddy with HTTPS, the
# daily job at 07:30, the pre-game check at 17:00 and a nightly data backup. Safe to run again:
# every step checks first.
#
#   sudo DOMAIN=fantasy.example.com REPO_URL=https://github.com/you/fantasy_manager.git bash install.sh
#   sudo DOMAIN=fantasy.example.com bash /opt/fantasy_manager/deploy/install.sh    # code already copied
#
# Variables (all optional):
#   DOMAIN     your domain; writes /etc/caddy/Caddyfile for it (else Caddy is left alone)
#   REPO_URL   git URL to clone when APP_DIR doesn't exist yet (else this script's own checkout is copied)
#   BRANCH     branch to check out on first clone (default: the repo's default branch)
#   APP_DIR    install location (default /opt/fantasy_manager)
#   TIMEZONE   e.g. America/Toronto; sets the server clock zone used by the 07:30 and 17:00 timers
#   ENABLE_UFW=1  open 22/80/443 in ufw and turn the firewall on (off by default: see docs/hosting.md)
#
# Layout: code + venv owned by root (the services can't modify them), data/ owned by the
# `fantasy` service user (0700), .env owned root:fantasy (0640).
set -euo pipefail

APP_DIR="${APP_DIR:-/opt/fantasy_manager}"
APP_USER="fantasy"
APP_HOME="/var/lib/fantasy"
BACKUP_DIR="/var/backups/fantasy_manager"
PYTHON="python3.12"
SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UNITS=(fantasy-web.service fantasy-daily.service fantasy-daily.timer fantasy-pregame.service fantasy-pregame.timer
       fantasy-backup.service fantasy-backup.timer)

say()  { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33mWARNING: %s\033[0m\n' "$*" >&2; }
die()  { printf '\033[1;31mERROR: %s\033[0m\n' "$*" >&2; exit 1; }

[[ $EUID -eq 0 ]] || die "run as root: sudo bash $0"
if [[ -r /etc/os-release ]]; then
  . /etc/os-release
  [[ "${ID:-}" == "ubuntu" && "${VERSION_ID:-}" == "24.04" ]] \
    || warn "tested on Ubuntu 24.04; this is ${PRETTY_NAME:-unknown}. Continuing."
fi

# --------------------------------------------------------------------------- packages
say "Installing system packages"
export DEBIAN_FRONTEND=noninteractive
apt-get update -q
apt-get install -y -q "$PYTHON" "${PYTHON}-venv" git curl ca-certificates gnupg sqlite3 \
  debian-keyring debian-archive-keyring apt-transport-https

if ! command -v caddy >/dev/null 2>&1; then
  say "Installing Caddy from its official apt repository"
  keyring=/usr/share/keyrings/caddy-stable-archive-keyring.gpg
  [[ -f $keyring ]] || curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | gpg --dearmor -o "$keyring"
  chmod o+r "$keyring"
  [[ -f /etc/apt/sources.list.d/caddy-stable.list ]] || \
    curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' > /etc/apt/sources.list.d/caddy-stable.list
  chmod o+r /etc/apt/sources.list.d/caddy-stable.list
  apt-get update -q
  apt-get install -y -q caddy
else
  echo "caddy already installed ($(caddy version | head -n1))"
fi

if [[ -n "${TIMEZONE:-}" ]]; then
  say "Setting the time zone to $TIMEZONE"
  timedatectl set-timezone "$TIMEZONE"
fi

# --------------------------------------------------------------------------- user
if ! id "$APP_USER" >/dev/null 2>&1; then
  say "Creating the '$APP_USER' service user"
  useradd --system --create-home --home-dir "$APP_HOME" --shell /usr/sbin/nologin "$APP_USER"
else
  echo "user $APP_USER exists"
fi

# --------------------------------------------------------------------------- code
say "Getting the code into $APP_DIR"
if [[ -d "$APP_DIR/.git" ]]; then
  if [[ "$SRC_DIR" != "$APP_DIR" && -z "${REPO_URL:-}" ]]; then
    echo "$APP_DIR already exists; using it"
  fi
  if git -C "$APP_DIR" remote get-url origin >/dev/null 2>&1; then
    git -C "$APP_DIR" pull --ff-only || warn "git pull failed; continuing with the code already there"
  else
    echo "no git remote configured; skipping git pull"
  fi
elif [[ -e "$APP_DIR" && "$SRC_DIR" != "$APP_DIR" ]]; then
  [[ -f "$APP_DIR/pyproject.toml" ]] || die "$APP_DIR exists but is not a Fantasy Manager checkout"
  echo "$APP_DIR exists (not a git checkout); using it as is"
elif [[ ! -e "$APP_DIR" && -n "${REPO_URL:-}" ]]; then
  git clone ${BRANCH:+--branch "$BRANCH"} "$REPO_URL" "$APP_DIR"
elif [[ ! -e "$APP_DIR" ]]; then
  [[ -f "$SRC_DIR/pyproject.toml" ]] || die "set REPO_URL, or run this script from a copy of the repo"
  echo "copying $SRC_DIR (without .venv, data, .env)"
  mkdir -p "$APP_DIR"
  tar -C "$SRC_DIR" --exclude=./.venv --exclude=./data --exclude=./.env --exclude='./.env.*' \
      --exclude='__pycache__' -cf - . | tar -C "$APP_DIR" -xf -
fi
[[ -f "$APP_DIR/pyproject.toml" ]] || die "no pyproject.toml in $APP_DIR"
find "$APP_DIR" -path "$APP_DIR/data" -prune -o -path "$APP_DIR/.env" -prune -o -exec chown root:root {} +
chmod 755 "$APP_DIR"
# A tree copied from Windows may have CRLF line endings, which bash and systemd reject.
for f in "$APP_DIR"/deploy/*; do
  if grep -q $'\r' "$f"; then sed -i 's/\r$//' "$f"; echo "fixed line endings: $f"; fi
done

# --------------------------------------------------------------------------- venv
say "Python virtual environment"
[[ -x "$APP_DIR/.venv/bin/python" ]] || "$PYTHON" -m venv "$APP_DIR/.venv"
"$APP_DIR/.venv/bin/python" -m pip install -q --upgrade pip
"$APP_DIR/.venv/bin/python" -m pip install -q -e "$APP_DIR[web]"
"$APP_DIR/.venv/bin/python" -m compileall -q "$APP_DIR/fantasy_manager" >/dev/null || true
"$APP_DIR/.venv/bin/fm" --help >/dev/null && echo "fm installed: $APP_DIR/.venv/bin/fm"

# --------------------------------------------------------------------------- data, .env, backups
say "Data directory, .env and backup folder"
install -d -m 700 -o "$APP_USER" -g "$APP_USER" "$APP_DIR/data" "$APP_DIR/data/logs"
chown -R "$APP_USER:$APP_USER" "$APP_DIR/data"
install -d -m 700 -o "$APP_USER" -g "$APP_USER" "$BACKUP_DIR"
if [[ ! -f "$APP_DIR/.env" ]]; then
  cp "$APP_DIR/.env.example" "$APP_DIR/.env"
  ENV_CREATED=1
  echo "created $APP_DIR/.env from .env.example: fill it in (see docs/hosting.md)"
fi
sed -i 's/\r$//' "$APP_DIR/.env"      # a .env copied from Windows may have CRLF line endings
chown root:"$APP_USER" "$APP_DIR/.env"
chmod 640 "$APP_DIR/.env"

# --------------------------------------------------------------------------- systemd units
say "Installing systemd units"
changed=0
for unit in "${UNITS[@]}"; do
  tmp="$(mktemp)"
  sed "s#/opt/fantasy_manager#${APP_DIR}#g" "$APP_DIR/deploy/$unit" > "$tmp"
  if ! cmp -s "$tmp" "/etc/systemd/system/$unit"; then
    install -m 644 "$tmp" "/etc/systemd/system/$unit"
    echo "installed $unit"
    changed=1
  fi
  rm -f "$tmp"
done
(( changed )) && systemctl daemon-reload

# A root-side `fm` shortcut that runs as the service user from the right folder.
cat > /usr/local/bin/fm <<EOF
#!/bin/sh
# Run Fantasy Manager as the '$APP_USER' user from $APP_DIR (installed by deploy/install.sh).
cd "$APP_DIR" && exec sudo -u $APP_USER "$APP_DIR/.venv/bin/fm" "\$@"
EOF
chmod 755 /usr/local/bin/fm

# --------------------------------------------------------------------------- caddy
if [[ -n "${DOMAIN:-}" ]]; then
  say "Configuring Caddy for $DOMAIN"
  tmp="$(mktemp)"
  sed "s#fantasy.example.com#${DOMAIN}#g" "$APP_DIR/deploy/Caddyfile" > "$tmp"
  if ! cmp -s "$tmp" /etc/caddy/Caddyfile; then
    [[ -f /etc/caddy/Caddyfile && ! -f /etc/caddy/Caddyfile.orig ]] && cp /etc/caddy/Caddyfile /etc/caddy/Caddyfile.orig
    caddy validate --config "$tmp" --adapter caddyfile >/dev/null || die "the generated Caddyfile is invalid"
    install -m 644 "$tmp" /etc/caddy/Caddyfile
    echo "wrote /etc/caddy/Caddyfile"
  fi
  rm -f "$tmp"
  systemctl enable caddy >/dev/null
  systemctl reload-or-restart caddy
else
  warn "DOMAIN not set: Caddy not configured. Re-run with DOMAIN=your.domain once the A record points here."
fi

if [[ "${ENABLE_UFW:-0}" == "1" ]] && command -v ufw >/dev/null 2>&1; then
  say "Firewall (ufw): allowing SSH, HTTP, HTTPS"
  ufw allow OpenSSH >/dev/null
  ufw allow 80/tcp >/dev/null
  ufw allow 443/tcp >/dev/null
  ufw allow 443/udp >/dev/null
  ufw --force enable
fi

# --------------------------------------------------------------------------- services
say "Enabling services and timers"
systemctl enable fantasy-web.service fantasy-daily.timer fantasy-pregame.timer fantasy-backup.timer >/dev/null
systemctl restart fantasy-web.service
systemctl start fantasy-daily.timer fantasy-pregame.timer fantasy-backup.timer
sleep 2
if curl -fsS --max-time 5 http://127.0.0.1:8765/healthz >/dev/null; then
  echo "dashboard is up on 127.0.0.1:8765"
else
  warn "dashboard did not answer yet: journalctl -u fantasy-web -n 50"
fi

# Presence checks only; values are never printed.
has_var() { grep -Eq "^[[:space:]]*$1=[^[:space:]]" "$APP_DIR/.env"; }
pw_note="(already set)"
has_var FM_WEB_PASSWORD || pw_note="NOT SET YET: until it is, the dashboard refuses every visitor from the internet (403)"
env_note=""
[[ ${ENV_CREATED:-0} == 1 ]] && env_note="(just created from .env.example)"

say "Done. Next steps"
cat <<EOF
  1. Fill in the settings (league ids, cookies, webhooks):   sudo nano $APP_DIR/.env  $env_note
  2. Set a dashboard password in that file:                   FM_WEB_PASSWORD=<long random passphrase>
     $pw_note
  3. Apply .env changes:                                      sudo systemctl restart fantasy-web
  4. Check the leagues load:                                  fm settings && fm --league fantrax settings
  5. Open https://${DOMAIN:-<your domain>}/ and log in.
  6. Try the daily job now:                                   sudo systemctl start fantasy-daily && journalctl -u fantasy-daily -n 20
  Timers:  systemctl list-timers 'fantasy-*'      Logs:  journalctl -u fantasy-web -f   and   $APP_DIR/data/logs/
  Update later with:                                          sudo bash $APP_DIR/deploy/update.sh
EOF
