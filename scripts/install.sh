#!/usr/bin/env bash
# install.sh -- deploy Telegram Agent onto an existing Linux server, installing
# only the parts that are missing. Idempotent: safe to re-run (it upgrades in
# place and never touches your data or .env).
#
#   scripts/install.sh                  # guided install with systemd
#   scripts/install.sh --status         # what is installed, is it healthy
#   scripts/install.sh --uninstall      # remove app (data is kept unless --purge)
#
# Options:
#   --prefix DIR     install root          (default /opt/telegram-agent)
#   --data-dir DIR   database location     (default /var/lib/telegram-agent)
#   --env-file PATH  reuse an existing .env instead of asking questions
#   --port N         dashboard port        (default 8080, bound to 127.0.0.1)
#   --no-systemd     skip the service; print a run command instead
#   --purge          with --uninstall: also delete data and env
#
# Existing server parts this script DETECTS and REUSES (never reconfigures):
#   nginx / caddy  -> prints a ready reverse-proxy block, keeps WEB_BIND local
#   an existing .env / database -> preserved untouched
#   systemd        -> installs a hardened unit; otherwise prints run instructions
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

PREFIX="/opt/telegram-agent"
DATA_DIR="/var/lib/telegram-agent"
ENV_FILE=""
PORT="8080"
NO_SYSTEMD=0
ACTION="install"
PURGE=0
SERVICE="telegram-agent"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --prefix) PREFIX="$2"; shift 2 ;;
    --data-dir) DATA_DIR="$2"; shift 2 ;;
    --env-file) ENV_FILE="$2"; shift 2 ;;
    --port) PORT="$2"; shift 2 ;;
    --no-systemd) NO_SYSTEMD=1; shift ;;
    --status) ACTION="status"; shift ;;
    --uninstall) ACTION="uninstall"; shift ;;
    --purge) PURGE=1; shift ;;
    -h|--help) grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1 (see --help)" >&2; exit 1 ;;
  esac
done

# Input hardening: this runs as root; every value that reaches other commands
# must be a plain number or a clean absolute path.
[[ "$PORT" =~ ^[0-9]+$ ]] || { echo "ERROR: --port must be numeric" >&2; exit 1; }
for d in "$PREFIX" "$DATA_DIR"; do
  [[ "$d" == /* && "$d" != *[[:space:]\"\'\;\&\|\$]* ]] || {
    echo "ERROR: '$d' must be a simple absolute path" >&2; exit 1; }
done
[[ -z "$ENV_FILE" || ( "$ENV_FILE" == /* && "$ENV_FILE" != *[[:space:]\"\'\;\&\|\$]* ) ]] || {
  echo "ERROR: --env-file must be a simple absolute path (no spaces/quotes/metachars)" >&2; exit 1; }
[[ -z "$ENV_FILE" || -f "$ENV_FILE" ]] || { echo "ERROR: --env-file $ENV_FILE not found" >&2; exit 1; }

have_systemd() { [[ -d /run/systemd/system ]] && command -v systemctl >/dev/null; }
service_active() { systemctl is-active --quiet "$SERVICE" 2>/dev/null; }

# ---------------------------------------------------------------- status

if [[ "$ACTION" == "status" ]]; then
  echo "=== telegram-agent status ==="
  if [[ -x "$PREFIX/telegram-agent" ]]; then
    echo "binary:     $PREFIX/telegram-agent (installed)"
  elif [[ -x "$ROOT/target/release/telegram-agent" ]]; then
    echo "binary:     $ROOT/target/release/telegram-agent (repo build, not installed)"
  else
    echo "binary:     not found"
  fi
  for f in "$PREFIX/.env" "$ROOT/.env" "/etc/telegram-agent/env"; do
    [[ -f "$f" ]] && echo "env:        $f"
  done
  if [[ -f "$DATA_DIR/agent.db" ]]; then
    echo "database:   $DATA_DIR/agent.db ($(du -h "$DATA_DIR/agent.db" | cut -f1))"
  else
    echo "database:   none at $DATA_DIR"
  fi
  if have_systemd; then
    if service_active; then echo "service:    active"
    elif systemctl is-enabled --quiet "$SERVICE" 2>/dev/null; then echo "service:    enabled but not running"
    else echo "service:    not installed"; fi
  else
    echo "service:    systemd not present"
  fi
  command -v curl >/dev/null && curl -s -o /dev/null --max-time 3 \
    -w "dashboard:  HTTP %{http_code} at http://127.0.0.1:${PORT}/login\n" \
    "http://127.0.0.1:${PORT}/login" || echo "dashboard:  not reachable on :$PORT"
  exit 0
fi

# ---------------------------------------------------------------- uninstall

if [[ "$ACTION" == "uninstall" ]]; then
  if have_systemd && systemctl list-unit-files | grep -q "^${SERVICE}.service"; then
    systemctl stop "$SERVICE" 2>/dev/null || true
    systemctl disable "$SERVICE" 2>/dev/null || true
    rm -f "/etc/systemd/system/${SERVICE}.service"
    systemctl daemon-reload
    echo "removed systemd service"
  fi
  rm -rf "$PREFIX"
  echo "removed $PREFIX"
  if [[ "$PURGE" == "1" ]]; then
    rm -rf "$DATA_DIR"; rm -f "/etc/telegram-agent/env"
    echo "purged data ($DATA_DIR) and env"
  else
    echo "kept data ($DATA_DIR) and env — re-run install.sh to restore, or pass --purge"
  fi
  exit 0
fi

# ---------------------------------------------------------------- install

[[ $EUID -eq 0 ]] || { echo "ERROR: run as root (sudo) to install system-wide" >&2; exit 1; }

# 1. Locate a binary: existing install, release build, or a dist tarball.
BIN=""
if [[ -x "$ROOT/target/release/telegram-agent" ]]; then
  BIN="$ROOT/target/release/telegram-agent"
elif compgen -G "$ROOT/dist/telegram-agent-*-linux-*.tar.gz" >/dev/null; then
  TARBALL="$(ls -1 "$ROOT"/dist/telegram-agent-*-linux-*.tar.gz | sort -V | tail -1)"
  TMP="$(mktemp -d)"; tar -xzf "$TARBALL" -C "$TMP"
  BIN="$(find "$TMP" -type f -name telegram-agent | head -1)"
  [[ -n "$BIN" ]] || { echo "ERROR: no binary in $TARBALL" >&2; exit 1; }
  trap 'rm -rf "$TMP"' EXIT
else
  echo "ERROR: no binary found. Run 'cargo build --release' or 'make package' first," >&2
  echo "       or download a release tarball into dist/." >&2
  exit 1
fi
echo "using binary: $BIN"

# 2. Service user.
if ! id telegram-agent >/dev/null 2>&1; then
  useradd --system --home-dir "$DATA_DIR" --shell /usr/sbin/nologin telegram-agent
  echo "created system user telegram-agent"
fi

# 3. Layout: app in $PREFIX, data in $DATA_DIR (kept across upgrades).
mkdir -p "$PREFIX" "$DATA_DIR"
if [[ -f "$PREFIX/telegram-agent" ]]; then
  systemctl stop "$SERVICE" 2>/dev/null || true
  cp "$BIN" "$PREFIX/telegram-agent.new" && mv "$PREFIX/telegram-agent.new" "$PREFIX/telegram-agent"
else
  cp "$BIN" "$PREFIX/telegram-agent"
fi
chmod 755 "$PREFIX/telegram-agent"
chown -R telegram-agent:telegram-agent "$DATA_DIR"

# 4. Environment: reuse existing, copy provided, or create from questions.
ENV_PATH="$PREFIX/.env"
if [[ -n "$ENV_FILE" ]]; then
  [[ -f "$ENV_FILE" ]] || { echo "ERROR: --env-file $ENV_FILE not found" >&2; exit 1; }
  [[ -f "$ENV_PATH" ]] && cp "$ENV_PATH" "$ENV_PATH.bak.$(date +%s)"
  cp "$ENV_FILE" "$ENV_PATH"
  echo "installed env from $ENV_FILE"
elif [[ -f "$ENV_PATH" ]]; then
  echo "keeping existing $ENV_PATH (backed up copies: *.bak.*)"
else
  read -rp "Telegram bot token (from @BotFather): " BOT_TOKEN
  read -rp "Owner Telegram user ID: " OWNER_ID
  read -rp "Dashboard password (min 6 chars): " WEB_PASS
  umask 077
  cat > "$ENV_PATH" <<EOF
TELEGRAM_BOT_TOKEN=${BOT_TOKEN}
OWNER_ID=${OWNER_ID}
DATABASE_DIR=${DATA_DIR}
WEB_PORT=${PORT}
WEB_BIND=127.0.0.1
WEB_USERNAME=owner
WEB_PASSWORD=${WEB_PASS}
OPEN_BROWSER=0
RUST_LOG=info
EOF
fi
chown telegram-agent:telegram-agent "$ENV_PATH"
chmod 600 "$ENV_PATH"

# 5. Reverse proxy awareness: if nginx/caddy exist, print matching blocks and
#    keep the app bound to localhost (unconditionally behind a proxy). We
#    never edit foreign configs.
PROXY_HINT=""
if command -v nginx >/dev/null; then
  PROXY_HINT="nginx"
elif command -v caddy >/dev/null; then
  PROXY_HINT="caddy"
fi
if [[ -n "$PROXY_HINT" ]]; then
  if grep -q '^WEB_BIND=' "$ENV_PATH"; then
    sed -i 's/^WEB_BIND=.*/WEB_BIND=127.0.0.1/' "$ENV_PATH"
  else
    printf 'WEB_BIND=127.0.0.1\n' >> "$ENV_PATH"
  fi
fi

# 6. systemd (or run instructions).
if have_systemd && [[ "$NO_SYSTEMD" == "0" ]]; then
  cat > "/etc/systemd/system/${SERVICE}.service" <<EOF
[Unit]
Description=Telegram Business Platform
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=telegram-agent
Group=telegram-agent
WorkingDirectory=$PREFIX
EnvironmentFile=$ENV_PATH
ExecStart=$PREFIX/telegram-agent
Restart=on-failure
RestartSec=5

# Hardening
NoNewPrivileges=true
PrivateTmp=true
ProtectHome=true
ProtectSystem=strict
ReadWritePaths=$DATA_DIR
ProtectKernelTunables=true
ProtectControlGroups=true
RestrictSUIDSGID=true

[Install]
WantedBy=multi-user.target
EOF
  systemctl daemon-reload
  systemctl enable --now "$SERVICE"
  sleep 2
  service_active && echo "service: running" || { journalctl -u "$SERVICE" -n 20 --no-pager; exit 1; }
else
  echo
  echo "systemd not used. Run the agent with:"
  echo "  systemctl start telegram-agent  # or: set -a; . $PREFIX/.env; set +a; $PREFIX/telegram-agent"
fi

# 7. What the operator should do next.
echo
echo "=== installed ==="
echo "  app:    $PREFIX/telegram-agent"
echo "  data:   $DATA_DIR   (back this up)"
echo "  env:    $ENV_PATH   (mode 600)"
have_systemd && echo "  manage: systemctl {status|restart|stop} $SERVICE"
if [[ "$PROXY_HINT" == "nginx" ]]; then
  cat <<EOF

  nginx detected — serve the dashboard over TLS with a block like:

    server {
        server_name agent.example.com;
        location / {
            proxy_pass http://127.0.0.1:${PORT};
            proxy_set_header Host \$host;
            proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
        }
        # TLS: certbot --nginx -d agent.example.com
    }
EOF
elif [[ "$PROXY_HINT" == "caddy" ]]; then
  cat <<EOF

  caddy detected — add this to your Caddyfile for automatic TLS:

    agent.example.com {
        reverse_proxy 127.0.0.1:${PORT}
    }
EOF
else
  echo
  echo "  no proxy detected — dashboard is on http://127.0.0.1:${PORT} (use an SSH tunnel to reach it:"
  echo "    ssh -L ${PORT}:127.0.0.1:${PORT} user@this-server )"
fi
echo
echo "health check: scripts/install.sh --status   · diagnostics: scripts/doctor.sh"
