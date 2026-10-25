# Architecture — Telegram Business Platform

Multi-tenant platform for sellers to run subscription businesses on Telegram:
one Rust binary, two interfaces (bot + web), one SQLite database.

**New here?** Read this page top-to-bottom (~5 min), then `docs/EASY_SETUP.md`
to run your own instance. **Working on the code?** Read `AGENT.md` (standards)
and `AUTO_ROUTE.md` (who does what) before your first change.

## Quick picture

```
┌─────────────────┐     ┌─────────────────┐
│  Telegram Bot   │     │  Web Dashboard  │
│  (teloxide)     │     │  (axum)         │
│  handlers/      │     │  web.rs         │
└────────┬────────┘     └────────┬────────┘
         │                       │
         └──────────┬────────────┘
                    │  role check: roles::role_of()
            ┌───────▼───────┐
            │ Domain Core   │  db.rs · crypto.rs · ai/ · dialogue.rs
            └───────┬───────┘
                    │  parameterized SQL, seller_id scoping
            ┌───────▼───────┐
            │  SQLite (WAL) │  data/agent.db
            └───────────────┘
```

Two thin interfaces, one core. Interfaces parse input and render output; every
business rule and every write lives in the core, behind `Db` methods. That is
the rule that keeps the bot and the dashboard from drifting apart.

## Modules

| Path | Owns | Rung |
|------|------|------|
| `src/main.rs` | bootstrap: db, bot task, web task, background jobs | L8 |
| `src/config.rs` | env config | L9 |
| `src/db.rs` | SQLite data layer — all persistence | L9 |
| `src/crypto.rs` | Crypto Pay HTTP client | L9 |
| `src/util.rs` | helpers | L9 |
| `src/roles.rs` | role resolution (Owner > Admin > Seller > User) | L8 |
| `src/dialogue.rs` | conversation state | L8 |
| `src/api.rs` | API surface | L8 |
| `src/ai/` | AI providers (OpenAI/Anthropic), cost, rate limits | L8 |
| `src/handlers/` | bot commands, messages, callbacks | L8 (callbacks: L7) |
| `src/web.rs` | axum routes, auth, HTML | L7 |

The Rung column maps each module to its owning role level — see
[Who builds it](#who-builds-it-role-level-routing) below.

## Request flow

**Telegram:** update → `handlers/` dispatch → `roles::role_of()` gate →
`dialogue.rs` state machine (wizards) → `Db` methods → reply / inline keyboard.

**Web:** request → `web.rs` route → session cookie auth → `Db` methods →
server-rendered HTML (no SPA, no client-side state).

**Payment:** `/buy` → invoice row → Crypto Pay → background job polls/verifies
via `crypto.rs` **server-side** → subscription granted only after API
confirmation. Never trust a user's claim of payment.

## Data model

Multi-tenant: every business entity carries `seller_id`; superadmin
(`OWNER_ID`) sees everything. Tables: `sellers`, `config`, `posts`, `invoices`,
`admins`, `plans`, `subscriptions`, `activation_codes`, `code_redemptions`,
`users`. Migrations run on startup (`Db::migrate` from `schema.sql`).

## Who builds it: role-level routing

Every change routes to a role level (a **blast-radius contract**, not a title):

| Ask | Rung | Agent |
|-----|------|-------|
| "Is this actually true?" | L3 | associate-verification (read-only) |
| "Write the docs." | L4 | junior-media-writer |
| "What should this cost?" | L5 | senior-finance (spec; L8 codes) |
| "Make it feel like one product." | L6 | senior-design-ux (+ L6-C charter) |
| "Implement it on the surface." | L7 | staff-client-platforms |
| "Build the bot flow." | L8 | staff-server |
| "Change data/crypto/build." | L9 | staff-core-systems |
| "Is this safe?" | L10 | principal-security (blocks; never patches) |
| "Where do domains meet?" | L11 | fellow-architect |

Full routing table, authority tiers (T0 act / T1 propose / T2 escalate), and
conflict rules: [`AUTO_ROUTE.md`](./AUTO_ROUTE.md). The ladder is
**machine-checked**:

```bash
python3 scripts/verify-agent-tiers.py
```

## The expanded system map — every rung owns a working subsystem

Each role level doesn't just review code; it owns a shipped, functional
subsystem. This is what "fully functional per level" means here:

| Rung | Subsystem (owned, shipped) |
|------|----------------------------|
| L11 | Architecture itself: this map, `AGENT.md`, `AUTO_ROUTE.md`, deployment profiles, the machine-checked ladder (`scripts/verify-agent-tiers.py`) |
| L10 | Security floor: role gating, server-side payment verification, hashed session cookies, `.env` 0600, secrets denied to every agent, supply-chain gates (`cargo audit`/`deny`) |
| L9 | Core platform & distribution: `db`/`crypto`/`config`, migrations, release profile, app icons, `package.sh`, `install.sh`, `doctor.sh`, Docker, 3-platform release CI |
| L8 | Application services: bot handlers, dialogue wizard, AI service, background jobs (payment verification, scheduled posts), setup-mode bootstrap |
| L7 | Interfaces: axum dashboard, bot inline surfaces, desktop browser auto-open |
| L6 (+L6-C) | Experience: `DESIGN.md` token contract, `design-system/`, a11y floors, icon generated from tokens |
| L5 | Monetization: pricing/billing spec (`docs/CRYPTO_PAY.md`), plans, invoices, activation codes, subscriptions |
| L4 | Docs & onboarding: README, `docs/` (setup, desktop, deployment profiles, commands), QUICKSTART inside every bundle |
| L3 | Verification: `make test` gate suite, `doctor.sh` diagnostics, smoke checks, routing-contract checks |

A task "expand level X" means: grow that row, keep the others' contracts
intact, and prove it with L3 before calling it done.

## Deployment profiles — the same binary anywhere

| Where | Entry point |
|-------|-------------|
| Desktop (Win/mac/Linux) | `make package` → run the bundle (see `docs/DESKTOP.md`) |
| New server | `./setup.sh` + `make run-release` |
| **Existing server** | `sudo make install` — installs only missing parts, reuses nginx/caddy/`.env`/data (`scripts/install.sh`) |
| Docker | `docker compose up -d --build` |
| ARM | arm64 release artifact / build on device |

Full matrix and operational details: [`docs/DEPLOY_PROFILES.md`](./docs/DEPLOY_PROFILES.md).
Everything is one `make` away — `make help` lists all targets.

## Security

- Role gating on every privileged action (`roles::role_of()`).
- Payments verified server-side via Crypto Pay API before granting access.
- Web session cookies hashed (SHA-256); secrets only via env, never logged.
- Parameterized SQL everywhere; all writes through `Db` methods.
- Patched teloxide (unmaintained aquamarine removed); `cargo audit`/`deny` in gates.

## Setup in three commands

```bash
./setup.sh                 # interactive: token, owner id, password → .env (0600)
cargo build --release      # or: docker compose up -d --build
./target/release/telegram-agent
```

Details: `docs/EASY_SETUP.md` · Operations: `docs/DEPLOYMENT.md` ·
Security: `docs/SECURITY.md`.

## Distribution: server or desktop app

One codebase, two deployment shapes:

- **Server** — Docker Compose / systemd; dashboard on `WEB_BIND=0.0.0.0`
  behind a TLS proxy. See `docs/DEPLOYMENT.md`.
- **Desktop app** — the same binary packaged per platform with
  `scripts/package.sh`: `telegram-agent.exe` (embedded icon/version resources)
  on Windows, `TelegramAgent.app` bundle on macOS, tar.gz on Linux. The
  dashboard binds `127.0.0.1` and opens in the system browser automatically.
  Data resolves portable-first (`.env`/`data/` beside the binary), else the
  platform dir (`%APPDATA%`, `~/Library/Application Support`, `~/.local/share`).
  Release CI (`.github/workflows/release.yml`) builds all three on a `v*` tag.
  See `docs/DESKTOP.md`.

## Production checklist

- [ ] `.env` mode 600; bot token and dashboard password set.
- [ ] `data/` backed up (it is the whole business state).
- [ ] `restart: unless-stopped` (compose) or systemd unit.
- [ ] Healthcheck green (`GET /`).
- [ ] `RUST_LOG=info`; alerts on payment-job errors.
- [ ] Quality gates green before every deploy (AUTO_ROUTE.md §7).

## Scalability

- **v1:** single SQLite (WAL + busy_timeout), single process — good for
  thousands of sellers. Command response < 2s; DB p99 < 50ms; < 64MB idle.
- **v2:** Postgres, horizontal bot workers, Redis cache, read replicas.

## Non-goals (v1)

No Kubernetes, no microservices, no ML, no chat beyond Telegram.
