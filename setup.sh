#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

echo "=== Telegram Business Platform setup ==="

# Defaults
ENV_FILE="${ENV_FILE:-.env}"
DB_PATH="${DB_PATH:-./data/agent.db}"

read -rp "Telegram bot token (from @BotFather): " BOT_TOKEN
while [[ -z "$BOT_TOKEN" ]]; do
  read -rp "Bot token is required: " BOT_TOKEN
done

# Validate bot token via Telegram API
echo "Validating bot token..."
if ! curl -fsS "https://api.telegram.org/bot${BOT_TOKEN}/getMe" >/dev/null 2>&1; then
  echo "ERROR: Telegram rejected the bot token." >&2
  exit 1
fi
echo "Bot token OK."

read -rp "Owner Telegram user ID (numeric): " OWNER_ID
while ! [[ "$OWNER_ID" =~ ^[0-9]+$ ]] || [[ "$OWNER_ID" == "0" ]]; do
  read -rp "Enter a positive numeric user ID: " OWNER_ID
done

read -rp "Default channel (optional, e.g. @yourchannel): " DEFAULT_CHANNEL
read -rp "Crypto Pay token (optional): " CRYPTO_TOKEN
read -rp "Web dashboard port [8080]: " WEB_PORT
WEB_PORT="${WEB_PORT:-8080}"
read -rp "Dashboard username [owner]: " WEB_USER
WEB_USER="${WEB_USER:-owner}"
read -rsp "Dashboard password (min 6 chars): " WEB_PASS
echo
while [[ ${#WEB_PASS} -lt 6 ]]; do
  read -rsp "Password must be at least 6 chars: " WEB_PASS
  echo
done

mkdir -p "$(dirname "$DB_PATH")"

# Write .env with restrictive permissions
umask 077
cat > "$ENV_FILE" <<EOF
TELEGRAM_BOT_TOKEN=${BOT_TOKEN}
OWNER_ID=${OWNER_ID}
DATABASE_URL=sqlite:${DB_PATH}
DEFAULT_CHANNEL=${DEFAULT_CHANNEL}
CRYPTO_PAY_TOKEN=${CRYPTO_TOKEN}
WEB_USERNAME=${WEB_USER}
WEB_PASSWORD=${WEB_PASS}
WEB_PORT=${WEB_PORT}
RUST_LOG=info
EOF
chmod 600 "$ENV_FILE"

echo "Wrote $ENV_FILE (mode 600)."

# Build?
if command -v cargo >/dev/null 2>&1; then
  read -rp "Build release binary now? [Y/n]: " BUILD
  BUILD="${BUILD:-Y}"
  if [[ "$BUILD" =~ ^[Yy]$ ]]; then
    cargo build --release
    echo "Build complete: ./target/release/telegram-agent"
  fi
fi

# Docker?
if command -v docker >/dev/null 2>&1; then
  read -rp "Start with Docker Compose now? [y/N]: " DOCK
  DOCK="${DOCK:-N}"
  if [[ "$DOCK" =~ ^[Yy]$ ]]; then
    docker compose up -d --build
    echo "Started. Open http://localhost:${WEB_PORT}"
  fi
fi

echo "Done. Run ./target/release/telegram-agent or 'docker compose up -d'."
