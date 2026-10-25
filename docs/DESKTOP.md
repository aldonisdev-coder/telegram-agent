# Desktop Distribution (.exe / .app / Linux)

Telegram Agent is a single Rust binary: the same code runs as a server
(Docker/systemd — see `DEPLOYMENT.md`) or as a desktop app on Windows, macOS,
and Linux. The dashboard opens automatically in your browser.

## What end users get

| Platform | Artifact | Contains |
|----------|----------|----------|
| Windows | `telegram-agent-<ver>-windows-x64.zip` | `telegram-agent.exe` (icon + version resources embedded), `.env.example`, quickstart |
| macOS | `telegram-agent-<ver>-macos-<arch>.zip` | `TelegramAgent.app` bundle (icon, Info.plist, ad-hoc signature) |
| Linux | `telegram-agent-<ver>-linux-<arch>.tar.gz` | binary, `.env.example`, icon, quickstart |

First run: unarchive, optionally create `.env` next to the binary (or configure
in the web UI), double-click / run. The dashboard opens at
`http://127.0.0.1:8080`.

## Where data lives (important)

- **Portable mode** — if a `.env` file or `data/` folder exists next to the
  binary, everything (database, `.env`) stays in that folder. This is the
  repo/development layout and the unzipped-bundle layout if you add a `.env`.
- **Platform mode** — otherwise the app uses the platform convention:
  - Windows: `%APPDATA%\telegram-agent`
  - macOS: `~/Library/Application Support/telegram-agent`
  - Linux: `$XDG_DATA_HOME/telegram-agent` (default `~/.local/share/telegram-agent`)
- `DATABASE_DIR` env overrides everything. `DATABASE_URL` still wins for the
  database file itself.

## Security defaults on desktop

- Dashboard binds to **127.0.0.1 only** — not reachable from the network.
  Set `WEB_BIND=0.0.0.0` only behind a TLS reverse proxy on a server.
- Browser auto-open: on by default for Windows/macOS, off for Linux;
  `OPEN_BROWSER=0` / `OPEN_BROWSER=1` overrides.

## Building packages yourself

```bash
python3 scripts/gen_icons.py      # regenerate assets/icon.{png,ico,icns}
scripts/package.sh                # host platform -> dist/
scripts/package.sh x86_64-pc-windows-gnu   # cross-compile (target must be installed)
```

Release CI (`.github/workflows/release.yml`) builds all three platforms on a
`v*` git tag and attaches the artifacts to a GitHub release.

### Regenerating icons

Icons are generated from the `DESIGN.md` brand tokens (accent `#0071e3`,
paper-plane glyph) with stdlib Python — no design tooling required:
`python3 scripts/gen_icons.py` writes `assets/icon.png` (512 master),
`assets/icon.ico` (16–256), `assets/icon.icns` (128/256/512).

## Code signing & Gatekeeper (distributing publicly)

- **macOS:** the packaged `.app` carries an *ad-hoc* signature (runs locally).
  For distribution outside your machines you need an Apple Developer ID:
  `codesign --sign "Developer ID Application: …" --options runtime` plus
  `xcrun notarytool submit`. Until then users right-click → Open the first time.
- **Windows:** the `.exe` has the icon and version block embedded (via
  `build.rs` + `winresource`). For no SmartScreen warnings, sign with an
  Authenticode certificate via `signtool`.
- **Linux:** binary is self-contained (bundled SQLite, static TLS where
  possible); AppImage wrapping is optional and left to distributors.

## Auto-start on login (optional)

- **Windows:** Task Scheduler → "At log on" → action: path to `telegram-agent.exe`.
- **macOS:** System Settings → General → Login Items → add `TelegramAgent.app`.
- **Linux:** `~/.config/autostart/telegram-agent.desktop` with
  `Exec=/path/to/telegram-agent`, or a systemd **user** service.
