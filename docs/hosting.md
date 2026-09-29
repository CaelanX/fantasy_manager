# Hosting the dashboard on a server

This guide puts Fantasy Manager on a small rented Linux server so that:

- the dashboard is at `https://fantasy.yourdomain.com` on your phone from anywhere, behind a password;
- the daily job (`fm auth fantrax --ping`, `fm backtest archive`, `fm harness daily`, `fm report --notify`) runs at 07:30 every morning without your PC being on;
- the data folder is backed up every night.

You don't need to be a Linux expert. You'll copy and paste about a dozen commands. Plan on about an hour the first time, most of it waiting for DNS.

If you'd rather not expose anything to the internet, read [Tailscale instead of a public domain](#tailscale-instead-of-a-public-domain) first. It skips the domain, Caddy and the open ports.

## What it costs

| Item | Typical price | Notes |
|---|---|---|
| VPS, 2 GB RAM (recommended) | US$5-12 / month | Hetzner is cheapest; DigitalOcean and Vultr are close |
| VPS, 1 GB RAM (works, add swap) | US$4-6 / month | See [Low memory](#low-memory-1-gb-servers) |
| Domain name | US$10-20 / year | `.com` about $10-12 at Cloudflare or Porkbun; renewals vary by extension |
| Provider backups (optional) | about 20% of the VPS price | Hetzner, DigitalOcean and Vultr all offer daily or weekly snapshots |
| HTTPS certificate | free | Caddy gets and renews Let's Encrypt certificates itself |
| Tailscale (alternative) | free for personal use | No domain needed |

Expect **about US$6-15 a month** in total. Prices change, so check the provider's page.

## How it fits together

```
phone/PC --HTTPS--> Caddy (ports 80/443, certificates, security headers)
                      \--> uvicorn on 127.0.0.1:8765 (fantasy-web.service, user "fantasy")
systemd timers: fantasy-daily.timer 07:30 -> deploy/daily.sh
                fantasy-backup.timer 03:15 -> deploy/backup.sh -> /var/backups/fantasy_manager
```

- Code and virtualenv: `/opt/fantasy_manager`, owned by root. The services can't modify them.
- Data (cache, harness ledger, archives, prefs, Fantrax session, logs): `/opt/fantasy_manager/data`, owned by the `fantasy` service user. This is the only folder the services can write to.
- Settings: `/opt/fantasy_manager/.env`, readable by root and the `fantasy` group only.
- The dashboard listens on `127.0.0.1` only. Nothing reaches it except through Caddy.

## 1. Rent a server

Any provider works. These three are simple and cheap:

- **Hetzner Cloud** (hetzner.com/cloud): the best value. Pick a shared-vCPU plan with 2 vCPU and 2-4 GB RAM. It has US and EU locations.
- **DigitalOcean** (digitalocean.com): a "Basic" Droplet, Regular, 1 GB ($6) or 2 GB ($12).
- **Vultr** (vultr.com): "Cloud Compute", Regular, 1 GB or 2 GB.

When you create the server:

1. **Image:** Ubuntu **24.04 LTS**. The install script expects it.
2. **Size:** 2 GB RAM recommended, 1 GB minimum. 25 GB of disk is plenty.
3. **Location:** close to you. It doesn't matter much.
4. **Authentication:** choose **SSH key**. On Windows, open PowerShell and run:

   ```powershell
   ssh-keygen -t ed25519            # press Enter to accept the defaults; a passphrase is optional
   Get-Content $HOME\.ssh\id_ed25519.pub
   ```

   Paste the line it prints (it starts with `ssh-ed25519`) into the provider's "SSH key" box.
5. Write down the server's **IPv4 address** (for example `203.0.113.25`).

## 2. Get a domain and point it at the server

1. Buy a domain from any registrar (Cloudflare Registrar, Porkbun and Namecheap are all fine). You can also use a subdomain of a domain you already own.
2. In the registrar's **DNS** settings, add an **A record**:

   | Type | Name | Value | TTL |
   |---|---|---|---|
   | A | `fantasy` | your server's IPv4 address | Auto / 300 |

   This makes `fantasy.yourdomain.com` point at the server. Use `@` as the name to use the bare domain instead.
3. **Cloudflare DNS users:** set the record to **DNS only** (grey cloud) so Caddy can get its certificate directly.
4. Wait until the name resolves (usually a few minutes, sometimes up to an hour). Check it from PowerShell:

   ```powershell
   Resolve-DnsName fantasy.yourdomain.com
   ```

   The address it prints must be your server's IP.

## 3. Connect with SSH

From PowerShell on your PC:

```powershell
ssh root@203.0.113.25
```

Type `yes` the first time. You are now typing commands **on the server** (the prompt changes to something like `root@ubuntu:~#`). `exit` disconnects. Every command below that starts with `sudo` runs on the server. Commands marked **PowerShell** run on your PC.

Update the server first:

```bash
sudo apt-get update && sudo apt-get -y upgrade
```

Ubuntu installs security updates by itself every day (`unattended-upgrades`). Reboot now and then with `sudo reboot`; the services start again on their own.

## 4. Put the code on the server

Choose **A** if your project is on GitHub (updates become one command). Otherwise choose **B**.

### A. Clone from GitHub (recommended)

For a **private** repository, give the server a read-only deploy key:

```bash
sudo ssh-keygen -t ed25519 -N "" -f /root/.ssh/fantasy_deploy
sudo cat /root/.ssh/fantasy_deploy.pub
printf 'Host github.com\n  IdentityFile /root/.ssh/fantasy_deploy\n' | sudo tee -a /root/.ssh/config
```

On GitHub, go to the repository > **Settings > Deploy keys > Add deploy key**, paste the printed line, and leave "Allow write access" **unchecked**. Then clone:

```bash
sudo apt-get install -y git
sudo git clone git@github.com:<you>/fantasy_manager.git /opt/fantasy_manager
```

For a public repository, use `https://github.com/<you>/fantasy_manager.git` instead and skip the deploy key.

### B. Copy it from your PC

In **PowerShell**, in your project folder, pack the code without the virtualenv, data or secrets, then copy it up:

```powershell
cd C:\code\fantasy_manager
tar --exclude=.venv --exclude=data --exclude=.env --exclude=__pycache__ -czf $env:TEMP\fm.tgz .
scp $env:TEMP\fm.tgz root@203.0.113.25:/root/fm.tgz
```

On the server, unpack it. Step 5 copies it into `/opt/fantasy_manager`:

```bash
mkdir -p /root/fantasy_manager && tar -xzf /root/fm.tgz -C /root/fantasy_manager
```

## 5. Run the installer

Replace the domain and time zone with yours. `TIMEZONE` decides what "07:30" means; the list is at `timedatectl list-timezones`.

```bash
# Option A (cloned to /opt/fantasy_manager):
sudo DOMAIN=fantasy.yourdomain.com TIMEZONE=America/Toronto bash /opt/fantasy_manager/deploy/install.sh

# Option B (unpacked to /root/fantasy_manager; the script copies it to /opt/fantasy_manager):
sudo DOMAIN=fantasy.yourdomain.com TIMEZONE=America/Toronto bash /root/fantasy_manager/deploy/install.sh
```

The installer is safe to run again at any time. It:

1. installs Python 3.12, git, SQLite and Caddy (from Caddy's official apt repository);
2. creates the `fantasy` system user (no login shell);
3. creates `/opt/fantasy_manager/.venv` and runs `pip install -e .[web]`;
4. creates `data/` (mode 0700) and `/var/backups/fantasy_manager`, and copies `.env.example` to `.env` if there is no `.env` yet;
5. installs and enables `fantasy-web.service`, `fantasy-daily.timer` and `fantasy-backup.timer`;
6. writes `/etc/caddy/Caddyfile` for your domain (the old one is kept as `Caddyfile.orig`), checks it and reloads Caddy;
7. adds a `fm` shortcut (`/usr/local/bin/fm`) that runs the tool as the `fantasy` user from the right folder;
8. prints the next steps.

**Firewall.** Most VPS images start with every port open. Only SSH (22) and Caddy (80, 443) should be reachable; the dashboard itself listens on 127.0.0.1 only. To turn on Ubuntu's firewall with just those ports open, add `ENABLE_UFW=1` to the install command, or run:

```bash
sudo ufw allow OpenSSH && sudo ufw allow 80/tcp && sudo ufw allow 443 && sudo ufw --force enable
```

If you moved SSH to another port, allow that port first or you'll lock yourself out. Hetzner and DigitalOcean also offer a firewall in their web console that does the same job.

## 6. Create `.env` on the server

The server needs the same settings as your PC: league ids, ESPN cookies, the Fantrax login or cookie, the OpenRouter key and the webhooks. **Never commit `.env` to git** (`.gitignore` already excludes it) and never paste it into chat or an issue.

**Copy your existing file** (in **PowerShell**):

```powershell
scp C:\code\fantasy_manager\.env root@203.0.113.25:/opt/fantasy_manager/.env
```

**Or type it in** on the server:

```bash
sudo nano /opt/fantasy_manager/.env       # Ctrl+O, Enter to save; Ctrl+X to quit
```

Then add the dashboard password. Generate a long random one:

```bash
openssl rand -base64 24
```

Put it in `.env`:

```
FM_WEB_PASSWORD=paste-the-generated-value-here
```

Save it in your password manager too. Leave `FM_DATA_DIR=./data` as it is: the services may only write to `/opt/fantasy_manager/data`. Keep each value on one line. systemd reads this file as well, and it doesn't expand `$`.

Fix the file's owner and permissions, then restart the dashboard to apply the changes:

```bash
sudo chown root:fantasy /opt/fantasy_manager/.env && sudo chmod 640 /opt/fantasy_manager/.env
sudo systemctl restart fantasy-web
```

Rerunning `install.sh` also fixes the permissions, and it converts Windows line endings.

Check that both leagues load:

```bash
fm settings
fm --league fantrax settings
fm auth fantrax --status
```

**Without `FM_WEB_PASSWORD` the dashboard refuses every visitor from the internet** (403, "only answers on this machine"). It fails closed: forgetting the password can't expose your data.

## 7. First login

Open `https://fantasy.yourdomain.com` on your phone or PC. The first visit can take a few seconds while Caddy gets the certificate. You'll see the **Log in** page. Enter `FM_WEB_PASSWORD` and you'll land on the overview. The first page load of a league takes 10-20 seconds, as it does locally.

- On your phone, use your browser's **Share > Add to Home Screen** to get an app-like icon.
- A login lasts 30 days on that browser (`FM_WEB_SESSION_DAYS`). **Log out** is at the bottom of every page.
- After 5 wrong passwords from one IP address, that address must wait 15 minutes. Failed attempts are logged (never the password): `journalctl -u fantasy-web | grep "login failed"`.
- **To log every device out**, change `FM_WEB_PASSWORD` (or delete `/opt/fantasy_manager/data/web_secret`) and run `sudo systemctl restart fantasy-web`.
- `/healthz` is the only page that needs no login. It returns `{"status": "ok", ...}` and no league data.

Everything else needs the login: every page, the `/api/*.json` endpoints, the mode toggle, **Refresh data**, **Explain**, **Rollback** and the setup page shown when credentials are missing.

## 8. Try the daily job

```bash
sudo systemctl start fantasy-daily          # runs now; takes a minute or two
journalctl -u fantasy-daily -n 20 --no-pager
```

Each step prints `ok` or `FAILED` with its log file. The full output is in `/opt/fantasy_manager/data/logs/` (`auth.log`, `archive.log`, `harness.log`, `report.log`). A failed step doesn't stop the later steps. The job is then marked failed, so you'll see it in `systemctl --failed`. When the timer runs next:

```bash
systemctl list-timers 'fantasy-*'
```

## Updating

When there's new code:

```bash
# Option A (git):
sudo bash /opt/fantasy_manager/deploy/update.sh
```

It pulls, reinstalls the package, refreshes the systemd units if they changed, restarts the dashboard and checks that it answers. Your `.env` and `data/` are never touched.

For option B, copy a fresh `fm.tgz` up as in step 4 (**PowerShell**), then on the server:

```bash
sudo tar -xzf /root/fm.tgz -C /opt/fantasy_manager && sudo bash /opt/fantasy_manager/deploy/update.sh
```

## Backups

`fantasy-backup.timer` runs `deploy/backup.sh` every night around 03:15. It writes `/var/backups/fantasy_manager/fantasy-data-YYYYmmdd-HHMMSS.tar.gz` and keeps the newest 14. SQLite databases (the harness ledger and others) are copied with SQLite's backup API, so a running job can't corrupt the copy. The HTTP caches are skipped because they refill themselves. Backups contain your Fantrax session and the dashboard key, so they are readable by root only.

```bash
sudo systemctl start fantasy-backup                       # back up now
ls -lh /var/backups/fantasy_manager/
```

A backup on the same server doesn't survive losing the server. Now and then, pull one down to your PC (**PowerShell**):

```powershell
scp "root@203.0.113.25:/var/backups/fantasy_manager/fantasy-data-*.tar.gz" C:\backups\
```

You can also turn on the provider's automatic snapshots.

**Restore:**

```bash
sudo systemctl stop fantasy-web
sudo tar -xzf /var/backups/fantasy_manager/fantasy-data-20261101-031500.tar.gz -C /opt/fantasy_manager
sudo chown -R fantasy:fantasy /opt/fantasy_manager/data
sudo systemctl start fantasy-web
```

To move to a new server, run the installer there, copy `.env`, and restore the newest backup the same way.

## Refreshing the Fantrax cookie remotely

Fantrax has no API keys, so the server needs a logged-in session. The saved session is in `data/fantrax_session.json`. The daily `fm auth fantrax --ping` keeps it alive, and with `FANTRAX_USERNAME` / `FANTRAX_PASSWORD` set the tool logs in again by itself when it expires.

**Captcha caveat.** Fantrax's login page uses reCAPTCHA. A scripted login from a datacenter IP address (any VPS) is much more likely to be rejected than one from your home connection. When that happens, the error mentions `BAD_INTERACTION` and `auth.log` shows the failed ping. The tool never tries to get around the captcha. So **keep a copied browser cookie in `FANTRAX_COOKIE` as a fallback** even when you use the username and password: its `FX_RM` "remember me" part lasts a long time.

When the footer, the report or `auth.log` says the Fantrax session is no longer valid:

1. On your PC, log in at fantrax.com and copy the `Cookie` request header (README, *Fantrax setup*, step 3).
2. Update it on the server, either **by editing** (`sudo nano /opt/fantasy_manager/.env` and replace the `FANTRAX_COOKIE=` line) or **with a cookie file** from **PowerShell**. Save the header to `fantrax.cookie` first:

   ```powershell
   scp .\fantrax.cookie root@203.0.113.25:/opt/fantasy_manager/data/fantrax.cookie
   ```

   and put `FANTRAX_COOKIE_FILE=./data/fantrax.cookie` in `.env` once. Then fix its permissions: `sudo chown fantasy:fantasy /opt/fantasy_manager/data/fantrax.cookie && sudo chmod 600 /opt/fantasy_manager/data/fantrax.cookie`.
3. Drop the stale saved session, check the new one and restart the dashboard:

   ```bash
   fm auth fantrax --logout && fm auth fantrax --ping
   sudo systemctl restart fantasy-web
   ```

Delete `fantrax.cookie` from your PC afterwards. Treat it like a password.

## Monitoring

| What | Command |
|---|---|
| Is the dashboard running? | `systemctl status fantasy-web` |
| Dashboard log (live) | `journalctl -u fantasy-web -f` (Ctrl+C to stop) |
| Last daily run | `journalctl -u fantasy-daily --since today --no-pager` |
| Step logs | `tail -n 50 /opt/fantasy_manager/data/logs/report.log` (also `auth`, `archive`, `harness`) |
| When timers run next | `systemctl list-timers 'fantasy-*'` |
| Anything failed? | `systemctl --failed` |
| Failed logins | `journalctl -u fantasy-web --since "-7 days" \| grep "login failed"` |
| Caddy / certificates | `journalctl -u caddy -n 50 --no-pager`; access log `/var/log/caddy/fantasy-access.log` |
| Disk space | `df -h /` |
| Model health | the dashboard's **Health** tab, or `fm harness status` |

For an alert when the site is down, point a free uptime checker (UptimeRobot, Healthchecks.io, Better Stack) at `https://fantasy.yourdomain.com/healthz`. It's public and exposes no data. The daily report webhook is also a daily sign of life: if it doesn't arrive, check `journalctl -u fantasy-daily`.

## Security notes

- Only Caddy is exposed. The dashboard listens on `127.0.0.1:8765` and trusts `X-Forwarded-For` / `X-Forwarded-Proto` only from `127.0.0.1` (`--proxy-headers --forwarded-allow-ips=127.0.0.1`).
- The login cookie is signed (HMAC-SHA256), HttpOnly, SameSite=Lax and Secure over HTTPS. It is tied to the current password.
- Caddy adds HSTS, `X-Frame-Options: DENY`, `nosniff`, a strict Content-Security-Policy and `noindex`.
- The login rate limit is kept in memory, so it resets when the service restarts. Its real protection is a long random password, which is why step 6 generates one.
- Use SSH keys only. Once key login works, set `PasswordAuthentication no` in `/etc/ssh/sshd_config.d/50-cloud-init.conf` (or `sshd_config`) and run `sudo systemctl restart ssh`. `sudo apt-get install fail2ban` adds SSH brute-force protection with no configuration.
- `fm web --host 0.0.0.0` (serving directly without Caddy) is refused without `FM_WEB_PASSWORD`. `FM_WEB_ALLOW_INSECURE=1` overrides that; use it only on a network you trust.

## Tailscale instead of a public domain

[Tailscale](https://tailscale.com) is a free (personal plan) private network between your own devices. The dashboard is then reachable only from your phone and PC, with nothing open to the internet: no domain, no Caddy, no ports 80/443.

1. Follow sections 1, 3 and 4 above (skip the domain).
2. Run the installer **without** `DOMAIN`, so Caddy stays unconfigured:

   ```bash
   sudo TIMEZONE=America/Toronto bash /opt/fantasy_manager/deploy/install.sh
   ```

3. Install Tailscale on the server and log in (it prints a link to open in your browser):

   ```bash
   curl -fsSL https://tailscale.com/install.sh | sh
   sudo tailscale up
   ```

4. Install the Tailscale app on your phone and PC and sign in with the same account.
5. In the Tailscale admin console under **DNS**, turn on **MagicDNS** and **HTTPS certificates**. Then share the dashboard inside your tailnet:

   ```bash
   sudo tailscale serve --bg 8765
   tailscale serve status        # shows https://<server-name>.<tailnet>.ts.net
   ```

6. Keep `FM_WEB_PASSWORD` set (recommended, in case a device on your tailnet is lost). Or, because only your tailnet can reach it, set `FM_WEB_ALLOW_INSECURE=1` instead. Then `sudo systemctl restart fantasy-web`.
7. Close the public ports if you opened them: `sudo ufw delete allow 80/tcp; sudo ufw delete allow 443`. You can even restrict SSH to Tailscale with `sudo ufw allow in on tailscale0 to any port 22` and `sudo ufw delete allow OpenSSH`, but only after confirming `ssh root@<server-name>` works over Tailscale.

Don't use `tailscale funnel`: it publishes the site to the whole internet.

## Docker alternative

If you prefer containers, `Dockerfile` and `docker-compose.yml` in the repo root run the same setup. The **web** container runs uvicorn, **caddy** provides HTTPS with the same `deploy/Caddyfile`, and a **cron** sidecar runs `deploy/daily.sh` at 07:30 and then `deploy/backup.sh`. On a server with Docker installed, in the project folder:

```bash
# in .env, besides your usual settings:  FM_DOMAIN=fantasy.yourdomain.com  TZ=America/Toronto  FM_WEB_PASSWORD=...
mkdir -p data backups && sudo chown -R 10001:10001 data backups     # the containers run as uid 10001
docker compose up -d --build
docker compose logs -f web                  # dashboard log; also: logs cron, logs caddy
docker compose exec web fm settings         # run any fm command
git pull && docker compose up -d --build    # update
```

The image never contains `.env` (`.dockerignore`); Compose passes it in at runtime. Compose treats `$` in `.env` values as a variable, so write a literal `$` as `$$`. Backups go to `./backups`. The Ubuntu + systemd path above remains the documented, primary one.

## Low memory (1 GB servers)

Add 2 GB of swap so a full league load can't run out of memory:

```bash
sudo fallocate -l 2G /swapfile && sudo chmod 600 /swapfile && sudo mkswap /swapfile && sudo swapon /swapfile
echo '/swapfile none swap sw 0 0' | sudo tee -a /etc/fstab
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| Browser says "This dashboard only answers on this machine" (403) | `FM_WEB_PASSWORD` isn't set, or the service wasn't restarted: edit `.env`, then `sudo systemctl restart fantasy-web` |
| Certificate or "can't connect" errors | DNS doesn't point at the server yet (`Resolve-DnsName`), or ports 80/443 are closed (ufw / provider firewall). See `journalctl -u caddy -n 50` |
| "Too many failed attempts" | Wait 15 minutes, or `sudo systemctl restart fantasy-web` |
| `$'\r': command not found` running a script | The file has Windows line endings: `sudo sed -i 's/\r$//' /opt/fantasy_manager/deploy/*.sh` (the installer does this for everything else) |
| `.env` changes don't show | `sudo systemctl restart fantasy-web` (the daily job reads `.env` fresh every run) |
| Setup page on the dashboard | A league credential is missing or expired; the page lists which ones (set/not set only, never the values) |
| Daily job failed | `journalctl -u fantasy-daily -n 30` and the step's log in `data/logs/` |
