# Deployment Profiles — the same app, anywhere

One codebase, one binary. Pick the profile that matches the machine you have;
every profile ends with the same three checks (`make doctor`, web dashboard,
Telegram bot answering `/start`).

| Profile | Best for | One command | Where data lives |
|---------|----------|-------------|------------------|
| **Desktop app** | Personal use, trying it out | `make package` → run the bundle | platform dir, or beside the binary (portable) |
| **New VPS** | A fresh cloud server | `make setup && make run-release` (or Docker below) | `./data` |
| **Existing server** | A server that already runs things | `sudo make install` | `/var/lib/telegram-agent` |
| **Docker** | Container hosts, NAS | `docker compose up -d --build` | `./data` bind mount |
| **ARM (Pi / ARM VPS)** | Raspberry Pi, ARM clouds | `make package` on the device (or grab the arm64 release) | same as host profile |

## Desktop app (Windows / macOS / Linux)

`make package` produces `dist/telegram-agent-<ver>-<platform>.<zip|tar.gz>`
(`telegram-agent.exe`, `TelegramAgent.app`, Linux tarball). Unarchive,
double-click; the dashboard opens at `http://127.0.0.1:8080`. Details:
[`DESKTOP.md`](./DESKTOP.md).

## New server (nothing installed yet)

```bash
git clone <repo> && cd telegram-agent
./setup.sh                 # validates your bot token, writes .env (0600)
make run-release           # or: make install  (systemd, hardened)
```

## Existing server (install only the missing parts)

`scripts/install.sh` is designed for machines that already run other things:

- **Reuses what exists**: an existing `.env` (`--env-file`), an existing
  database (data is never touched), nginx or caddy (prints a matching
  reverse-proxy block — it never edits foreign configs), systemd (installs a
  hardened unit: `ProtectSystem=strict`, `NoNewPrivileges`, restart on
  failure).
- **Installs only what's missing**: the binary to `/opt/telegram-agent`, a
  dedicated system user, the data dir `/var/lib/telegram-agent`, the service.
- **Idempotent**: re-running upgrades the binary in place; data and `.env`
  survive every upgrade.

```bash
sudo scripts/install.sh                          # guided
sudo scripts/install.sh --env-file /root/my.env --port 8181
sudo scripts/install.sh --status                 # health of the install
sudo scripts/install.sh --uninstall              # keeps data; --purge removes
```

With nginx already present you get a ready-to-paste TLS server block; with
caddy, a Caddyfile snippet. Without any proxy, the dashboard stays on
`127.0.0.1` and the script prints the SSH tunnel command to reach it.

## Docker

`docker compose up -d --build` — binds `0.0.0.0:8080` inside the container
(bridge it to a proxy or firewall it), healthcheck on `/`, `init: true`,
restart unless-stopped, data in `./data`.

## After any install: verify

```bash
make doctor        # binary, env, token (getMe), DB integrity, dashboard, service
make status        # install inventory on this machine
```

## Backup & restore (all profiles)

Everything of value is one directory: the data dir above (contains
`agent.db` + `dialogue.db`) plus your `.env`.

```bash
tar -czf backup-$(date +%F).tgz <data-dir> .env
```

Restore = unarchive to the same location and restart. Test restores
quarterly; an untested backup is a hope, not a backup.
