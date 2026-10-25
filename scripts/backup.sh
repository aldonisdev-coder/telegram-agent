#!/usr/bin/env bash
# backup.sh -- snapshot the SQLite database safely (WAL-aware).
#
# Uses sqlite3's online .backup when available (consistent even while the
# service is running); falls back to a plain file copy with a warning to stop
# the service first. Prints the backup path on success; exits non-zero on
# failure.
#
#   scripts/backup.sh          # writes backups/agent-<timestamp>.db (mode 600)
set -uo pipefail
cd "$(dirname "$0")/.." || { echo "ERROR: cannot locate repo root" >&2; exit 1; }
ROOT="$(pwd)"

# Locate the .env (same lookup as doctor.sh) and parse it WITHOUT sourcing
# (a crafted .env must never execute; export-assignment is safe).
ENV_PATH=""
for f in "$ROOT/.env" "/opt/telegram-agent/.env"; do
  [[ -f "$f" ]] && { ENV_PATH="$f"; break; }
done
if [[ -n "$ENV_PATH" ]]; then
  while IFS='=' read -r k v || [[ -n "$k" ]]; do
    case "$k" in
      [A-Z_]*) export "$k=$v" ;;
    esac
  done < "$ENV_PATH"
fi

# Resolve the database the same way doctor.sh does.
DATA="${DATABASE_DIR:-}"
[[ -z "$DATA" && -n "${DATABASE_URL:-}" ]] && DATA="${DATABASE_URL#sqlite:}"
[[ -z "$DATA" ]] && DATA="$ROOT/data"
DB="$DATA/agent.db"

[[ -f "$DB" ]] || { echo "ERROR: database not found at $DB" >&2; exit 1; }
mkdir -p "$ROOT/backups" || { echo "ERROR: cannot create $ROOT/backups" >&2; exit 1; }
DEST="$ROOT/backups/agent-$(date +%Y%m%d-%H%M%S).db"

fail() { echo "ERROR: $1" >&2; rm -f "$DEST"; exit 1; }

if command -v sqlite3 >/dev/null; then
  # WAL-safe online backup; safe while the service is running.
  sqlite3 "$DB" ".backup '$DEST'" || fail "sqlite3 .backup of $DB failed"
else
  echo "warn: sqlite3 not installed -- falling back to a raw file copy." >&2
  echo "      Stop the service first (systemctl stop telegram-agent) for a consistent snapshot." >&2
  cp -- "$DB" "$DEST" || fail "copy of $DB failed"
fi

[[ -s "$DEST" ]] || fail "backup $DEST is empty"
chmod 600 "$DEST" || { echo "ERROR: chmod 600 $DEST failed" >&2; exit 1; }
echo "$DEST"
