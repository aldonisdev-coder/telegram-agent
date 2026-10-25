# Easy Setup Experience — Phased Plan

## Goal
Non-technical business owners can deploy and configure the Telegram Business Platform
without editing `.env` or touching code. Single binary preserved. Secrets stay in
environment/`.env` (not DB) whenever possible; DB only holds non-secret config and
encrypted-at-rest placeholders if env is unavailable.

## Phase 0 — Defaults & validation (config.rs)
- `Config::from_env()` no longer hard-fails on missing `TELEGRAM_BOT_TOKEN`/`OWNER_ID`;
  returns a config with `is_configured() == false` so the app can boot into setup mode.
- Sensible defaults: `WEB_PORT=8080`, `WEB_USERNAME=owner`, DB path `sqlite:./data/agent.db`,
  `AI_MODEL=gpt-4o-mini`, `AI_BASE_URL=https://api.openai.com/v1`.
- Helpers: `Config::web_port()`, `Config::web_username()`, `Config::is_configured()`,
  `Config::web_url()`.
- Validation helpers (used by wizard): bot token validated against Telegram `getMe`;
  owner ID must be positive integer; channel must start with `@` or be numeric chat id;
  dashboard password >= 6 chars.

## Phase 1 — First-run web setup wizard (web.rs)
- Route `/setup` (GET/POST) available only when `!cfg.is_configured()`.
- `index` and `login_page` redirect to `/setup` in setup mode.
- Form collects: bot token, owner ID, default channel, crypto token, dashboard username/password.
- On submit:
  - Validate bot token via `https://api.telegram.org/bot<token>/getMe`.
  - Write `.env` atomically with mode 0600 (secrets remain in env file, NOT DB).
  - Seed owner web account + default channel in DB.
  - Show "restart required" page (auto-restart under Docker/systemd).
- Security: no secrets written to DB; `.env` chmod 600; rate-limit setup endpoint.

## Phase 2 — Setup-mode runtime (main.rs)
- Boot DB and web server even when unconfigured.
- Bot/dispatcher/background processors (invoice verification, scheduled posts) start only
  when `cfg.is_configured()`; otherwise a placeholder bot is used and a notice is logged.
- Web port from `cfg.web_port()`.
- Owner account seeding skipped in setup mode (wizard creates it).

## Phase 3 — Docker one-command deploy
- `Dockerfile`: multi-stage (rust:1.98 builder -> debian:bookworm-slim runtime), copies binary + schema.sql, exposes 8080.
- `docker-compose.yml`: builds image, maps `${WEB_PORT:-8080}:8080`, mounts `./data` and `./.env`,
  env_file optional, restart unless-stopped, healthcheck.
- First run: `docker compose up -d --build`, then open wizard at http://localhost:8080/setup.

## Phase 4 — setup.sh
- Interactive prompts with validation (bot token validated via curl getMe, owner ID numeric).
- Writes `.env` (mode 600), creates data dir.
- Optional: cargo build --release and/or docker compose up -d --build.

## Phase 5 — Hardening & UX (future)
- Encrypt secrets at rest in DB if env cannot be used (AES-256-GCM with key from `MASTER_KEY`
  env/volume), never plaintext.
- Auto-restart after wizard (process exit code 0 under Docker/systemd).
- In-app "Settings" page to rotate tokens (rewrites .env + prompts restart).
- Backup/restore helper for `data/` volume.

## Concrete file changes
- `src/config.rs`: `from_env()` -> `Self` (no hard failure), `is_configured()`, defaults, helpers.
- `src/main.rs`: setup-mode boot, conditional bot/dispatcher/background tasks, `cfg.web_port()`.
- `src/web.rs`: `/setup` GET/POST, redirects in setup mode, `validate_bot_token()`.
- New: `Dockerfile`, `docker-compose.yml`, `setup.sh`, `.env.example` (updated), `docs/EASY_SETUP.md`.

## Security notes
- Secrets (bot token, crypto token, dashboard password) are written only to `.env` (mode 0600)
  or environment; never stored in SQLite.
- Bot token validated before persisting; owner ID validated as positive integer.
- Setup endpoint should be disabled once configured (`is_configured()` guard).
- Single binary deployment preserved; no new runtime dependencies beyond reqwest/curl.
