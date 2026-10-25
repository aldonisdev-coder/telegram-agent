# AGENT.md — AI Engineering Handbook (Big-Tech Grade)

> Task routing, role levels, and skill activation: see `AUTO_ROUTE.md`.

## 0. Mission
This repository is a **multi-tenant Telegram business platform**. Any change must preserve:
- Multi-tenancy (seller_id scoping).
- Security (owner/admin gating, payment verification).
- Production readiness (tests, audit, docs).

## 1. Repository layout
```
src/
  main.rs          # bootstrap: db, bot task, web task, background jobs
  config.rs        # env config
  db.rs            # SQLite data layer (all persistence)
  crypto.rs        # Crypto Pay client
  util.rs          # helpers (to_chat_id, etc.)
  roles.rs         # role resolution (Owner > Admin > Seller > User)
  dialogue.rs      # conversation state
  api.rs           # API surface
  ai/              # AI providers, cost accounting, rate limits
  handlers/
    commands.rs    # Telegram command handler
    messages.rs    # photo wizard
    callbacks.rs   # inline buttons
  web.rs           # axum HTTP interface
vendor/
  teloxide/        # patched teloxide (aquamarine removed)
scripts/
  verify-agent-tiers.py  # machine-checks the role-level routing contract
docs/              # user & developer docs
.opencode/         # agents (role levels) + skills, dispatched via OpenCode
```

## 2. Build & quality gates (must pass before merge)
```bash
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt --check
cargo audit
cargo deny check
python3 scripts/verify-agent-tiers.py
```

## 3. Coding standards
- **Rust edition 2024**, stable toolchain.
- No `unwrap()` in production paths except startup; use `?` or match.
- All DB writes go through `Db` methods; no raw SQL in handlers.
- Handlers must check `roles::role_of()` (Owner > Admin > Seller > User) before acting.
- Scope all business data by `seller_id`.
- Use `channel_of(db, cfg, Some(seller_id))` for per-seller channels.
- Log via `tracing`; never log secrets.

## 4. Adding a feature
1. Update `schema.sql` + `Db::migrate`.
2. Add `Db` methods.
3. Add `Command` variant + handler arm + `setup_bot_profile` entry.
4. Update `docs/COMMANDS.md` and `README.md`.
5. Add tests.
6. Run quality gates.

## 5. Security checklist
- [ ] Owner/admin gating present.
- [ ] Payment verified via API before granting access.
- [ ] No secrets in code/logs.
- [ ] SQL parameterized (no string concat).
- [ ] Web auth cookie hashed.

## 6. Testing strategy
- Unit tests in `db.rs` for data layer.
- Property tests (proptest) for security-critical invariants: token shape/
  entropy, HMAC determinism + RFC 4231 vectors, ct_eq, invoice claim state
  machine (model-based), arbitrary `days` never panics, esc round-trip.
- Fuzzing (`fuzz/`, cargo-fuzz): `make fuzz` — libFuzzer over esc, hmac+ct_eq,
  invoice JSON parsing, gen_token. Found and fixed a decode-order bug in the
  esc test oracle on its first run.
- Integration tests with mock Telegram API (wiremock) in web/api tests.
- Manual smoke test: /start, /setup, /planadd, /buy, /paid, /redeem.

## 7. Deployment
- Single binary; env-based config.
- Systemd/Docker; see `docs/DEPLOYMENT.md`.
- Back up `data/agent.db`.

## 8. Observability
- `RUST_LOG=info` (or debug).
- Background job logs payment processing.

## 9. Performance budgets
- Command response < 2s.
- DB queries < 50ms (p99).
- Memory < 64MB idle.

## 10. Non-goals (v1)
- No Kubernetes, no microservices, no ML.
- No real-time chat beyond Telegram.

## 11. Decision log
- Patched teloxide to remove unmaintained aquamarine (RUSTSEC-2026-0173).
- SQLite over Postgres for v1 simplicity.
- Server-rendered HTML over SPA for low memory + security.
- Desktop distribution added: `scripts/package.sh` (.exe/.app/tar.gz); data dir
  resolves portable-first, then platform dir; dashboard binds 127.0.0.1 by
  default (`WEB_BIND` overrides; compose sets 0.0.0.0).
- Fixed startup panic: `dptree::case!` cannot bind struct-variant fields under
  teloxide's eager build() type-check — CryptoInvoice state now handled by a
  trailing catch-all endpoint (fields read from the dialogue in the handler).
- Fixed setup mode: unconfigured runs no longer start the poller (teloxide
  panics on placeholder-token getMe and killed the process with the web UI);
  setup mode is web-only until configured, then restart.
- 2026-10-25 security remediation (review docs/reviews/2026-10-25-architecture.md):
  CSPRNG tokens (getrandom) + one-time session/API-key rotation; /setup gated
  by DB `setup_done` (closes replay without restart); webhook signature
  mandatory + constant-time; payment claims atomic (`claim_invoice_for_grant`)
  and fail-closed with robust envelope parsing; auto-seller removed and
  seller channel scoping enforced; XSS escaping; shared login throttle
  (web+api); WAL-safe `make backup`; 6h expiry sweeper; MAX_PLAN_DAYS clamp
  with checked date math.
- 2026-10-25 plugin architecture (L11 design, L8 implement): `WEB_BASE_PATH`
  subpath mounting (`web::mount`, cookie/redirect base-prefixing — default ""
  behavior byte-identical), `embed::AgentAppBuilder` for embedding the
  dashboard/API into a host axum service, and a compile-time command registry
  (`handlers::registry`) so new bot commands are trait-registered instead of
  enum edits. Dynamic .so loading explicitly deferred (non-goal). Known issue:
  first-run setup rewrites .env with a fixed key list and drops WEB_BIND/
  WEB_BASE_PATH/WEB_URL — re-add after setup.
- 2026-10-25 v1.2 shipped (scale-plan approved as recommended): products +
  digital delivery + lifetime grants (product_grants table, UNIQUE invoice),
  multiplexed /buy + /shop, real refund/payout flows (owner-approved crypto
  transfer + audit + grant revoke/kick), .env rewrite preserves unknown keys,
  plan lookups seller-scoped, docs deduped. See
  docs/ARCHITECTURE-SCALE-PLAN.md implementation record.

## 12. Contacts
- Maintainer: repository owner.
- Issues: GitHub issues (when published).