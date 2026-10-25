#!/usr/bin/env bash
# package.sh -- build and package desktop distributions of Telegram Agent.
#
#   scripts/package.sh                 # build + bundle for the host platform
#   scripts/package.sh <rust-triple>   # cross-build for an installed target,
#                                      # e.g. x86_64-pc-windows-gnu
#
# Outputs into dist/:
#   Linux:   telegram-agent-<ver>-linux-<arch>.tar.gz
#   Windows: telegram-agent-<ver>-windows-<arch>.zip  (contains telegram-agent.exe)
#   macOS:   telegram-agent-<ver>-macos-<arch>.zip    (contains TelegramAgent.app)
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
TRIPLE="${1:-}"
DIST="dist"
mkdir -p "$DIST"

# ---------------------------------------------------------------- helpers

stage_name() { # $1=platform $2=arch
  echo "telegram-agent-${VERSION}-${1}-${2}"
}

write_quickstart() { # $1=staging dir, $2=run command
  cat > "$1/QUICKSTART.md" <<EOF
# Telegram Agent ${VERSION} -- quick start

1. If you have not configured yet: copy \`.env.example\` to \`.env\` next to this
   file, fill in \`TELEGRAM_BOT_TOKEN\` and \`OWNER_ID\` (from @BotFather and
   @userinfobot). You can also skip this and configure in the web UI.
2. Run: \`${2}\`
3. A browser opens at http://127.0.0.1:8080 (or set OPEN_BROWSER=0 to disable).

Where your data lives:
- If \`.env\` or a \`data/\` folder sits next to the binary -> portable mode,
  everything stays in that folder.
- Otherwise -> platform data dir:
  Windows  %APPDATA%\\telegram-agent
  macOS    ~/Library/Application Support/telegram-agent
  Linux    ~/.local/share/telegram-agent

Security: the dashboard binds to 127.0.0.1 only. To expose it on a network,
set WEB_BIND=0.0.0.0 and use a reverse proxy with TLS.
EOF
}

make_tarball() { # $1=staging dir
  tar -czf "${DIST}/${1}.tar.gz" "$1"
  echo "packaged ${DIST}/${1}.tar.gz"
}

make_zip() { # $1=staging dir or app name
  command -v zip >/dev/null || { echo "ERROR: 'zip' is required to package" >&2; exit 1; }
  zip -r -q "${DIST}/${1}.zip" "$1"
  echo "packaged ${DIST}/${1}.zip"
}

detect() { # sets PLATFORM (linux|windows|macos) and ARCH from triple or uname
  local t="${TRIPLE:-$(uname -m)-$(uname -s | tr '[:upper:]' '[:lower:]')}"
  case "$t" in
    *windows*) PLATFORM=windows ;;
    *apple*)   PLATFORM=macos ;;
    *linux*)   PLATFORM=linux ;;
    *)         echo "ERROR: unrecognized platform in '$t'" >&2; exit 1 ;;
  esac
  case "$t" in
    aarch64*|arm64*) ARCH=arm64 ;;
    x86_64*|amd64*)  ARCH=x64 ;;
    *)               ARCH="$(uname -m)" ;;
  esac
}

# ---------------------------------------------------------------- build

detect
echo "=== Telegram Agent ${VERSION} -- packaging for ${PLATFORM} (${ARCH}) ==="

if [[ -n "$TRIPLE" ]]; then
  cargo build --release --target "$TRIPLE"
  BIN="target/${TRIPLE}/release/telegram-agent"
else
  cargo build --release
  BIN="target/release/telegram-agent"
fi
if [[ "$PLATFORM" == "windows" && ! -f "${BIN}.exe" ]] \
  || [[ "$PLATFORM" != "windows" && ! -f "$BIN" ]]; then
  echo "ERROR: release binary not found" >&2
  exit 1
fi

case "$PLATFORM" in
  linux)
    STAGE="$(stage_name linux "$ARCH")"
    rm -rf "$STAGE"; mkdir -p "$STAGE"
    cp "$BIN" "$STAGE/telegram-agent"
    cp .env.example "$STAGE/"
    cp assets/icon.png "$STAGE/" 2>/dev/null || true
    write_quickstart "$STAGE" "./telegram-agent"
    make_tarball "$STAGE"
    ;;
  windows)
    EXE="${BIN}.exe"
    [[ -f "$EXE" ]] || { echo "ERROR: $EXE not found" >&2; exit 1; }
    STAGE="$(stage_name windows "$ARCH")"
    rm -rf "$STAGE"; mkdir -p "$STAGE"
    cp "$EXE" "$STAGE/telegram-agent.exe"
    cp .env.example "$STAGE/"
    cp assets/icon.ico "$STAGE/" 2>/dev/null || true
    write_quickstart "$STAGE" "telegram-agent.exe"
    make_zip "$STAGE"
    ;;
  macos)
    APP="TelegramAgent.app"
    STAGE="$(stage_name macos "$ARCH")"
    rm -rf "$APP" "$STAGE"; mkdir -p "$STAGE"
    mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
    cp "$BIN" "$APP/Contents/MacOS/telegram-agent"
    chmod +x "$APP/Contents/MacOS/telegram-agent"
    cp assets/icon.icns "$APP/Contents/Resources/icon.icns" 2>/dev/null || true
    cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Telegram Agent</string>
  <key>CFBundleDisplayName</key><string>Telegram Agent</string>
  <key>CFBundleIdentifier</key><string>io.github.telegram-agent</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleExecutable</key><string>telegram-agent</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF
    # Ad-hoc signature so the bundle runs locally; see docs/DESKTOP.md for
    # Developer ID + notarization when distributing outside the App Store.
    if command -v codesign >/dev/null; then
      codesign --force --sign - "$APP" >/dev/null 2>&1 || true
    fi
    cp -R "$APP" "$STAGE/"
    cp .env.example "$STAGE/"
    write_quickstart "$STAGE" "open TelegramAgent.app"
    make_zip "$STAGE"
    rm -rf "$APP"
    ;;
esac

echo "=== done: see dist/ ==="
