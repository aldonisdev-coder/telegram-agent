# Architecture Review & Plan — 2026-10-25

**Method.** Dispatched per `AUTO_ROUTE.md` §8 by the orchestrator: three
parallel read-only sub-agents — **L11 fellow-architect** (review + plan),
**L8 staff-server** (server-layer health), **L10 principal-security**
(blocking findings). Orchestrator verified the load-bearing citations against
the source (all confirmed verbatim) and integrated this document. L3 gate
status at time of review: green (fmt / clippy / 64 tests / audit / deny /
routing contract).

**Verdict (L11).** The core architecture is sound and the org/routing design
is real and machine-checked. The **security floor is not met** and money
correctness has races: per the conflict lattice (`AUTO_ROUTE.md` §5), those
findings gate the roadmap — everything else is queued behind them.

## Verified findings (all citations confirmed)

### Floor — blockers, fix before any feature work

| # | Finding | Evidence | Owner | Fix sketch |
|---|---------|----------|-------|------------|
| B1 | Session/API tokens from an LCG seeded by wall-clock nanos — forgeable offline ⇒ full admin takeover | `src/util.rs:42-55`; used at `db.rs:1257`, `db.rs:1573`, `handlers/commands.rs:973` | L9 | CSPRNG tokens; rotate all existing sessions/API keys/seller passwords after deploy |
| B2 | `/setup` unauthenticated and replayable (guard reads startup-frozen config until restart) | `src/web.rs:590-612`, `main.rs:31` | L8 | Persist "configured" in DB; close setup on completion; loopback-only in setup mode |
| B3 | Crypto Pay webhook accepts requests with no signature header (check is conditional) | `src/api.rs:3478-3494` | L8 | Mandatory header + configured token, constant-time compare, fail-closed |
| B4 | Invoice claim grants on API error or missing token (`if let Ok` fall-through) | `src/api.rs:3389-3409` | L8 | Fail-closed: grant only on explicit `status=="paid"`; fix `getInvoices` envelope parsing (`result.items`, flagged UNVERIFIED vs live API) |
| B5 | Every Telegram user auto-becomes a seller; `/grant` reaches the global paid channel | `handlers/commands.rs:118-120`, `:755-780`, `:1063-1072` | L8 | Remove unconditional `upsert_seller`; grant only into seller-owned channels |
| B6 | Payment grant is not atomic: three granters, no transaction, mark-paid-before-grant (crash = paid-never-granted; concurrency = double-grant) | `main.rs:143-193`, `commands.rs:592-644`, `api.rs:3370-3453`, `db.rs:536` | L8+L9 | One transactional `claim_and_grant`; UNIQUE(`subscriptions.invoice_id`) |
| B7 | `duration_days` overflow panics the unsupervised payment verifier (silent stop until restart) | `commands.rs:527`, `db.rs:769`, `main.rs:182` | L8 | Clamp 1..=3650; `checked_add_signed` everywhere |

### High (v1.1, immediately after the floor)

Owner web password defaults to the public `OWNER_ID` (`main.rs:44-53` — L8);
stored XSS via unescaped dashboard interpolation (`web.rs:352-422` — L7);
cross-seller channel writes, cross-seller `/edit`-family commands
(`api.rs:607-613`, `:1241-1265`, `:1519-1523`, `commands.rs:309-428` — L8);
self-marked "paid" invoices inflate payout revenue (`api.rs:739-758` — L8);
global plan namespace leaks pricing and mis-resolves `/buy` (`db.rs:724`,
`schema.sql:61` — L8/L9); API login unrated + web lockout map unbounded
(`api.rs:84`, `web.rs:247-265` — L8); `doctor.sh` sources `.env` as shell and
leaks the token via `ps` (`doctor.sh:46`, `:88` — L9); `install.sh` echoes
secrets and can be steered by a hostile `--env-file` (`install.sh:232`,
`:190-192` — L9); expiry only runs on manual `/expirecheck` (L8, S).

### Hygiene (v1.1 backlog)

`Mutex<Connection>::lock().unwrap()` on poisoned mutex (L9); bcrypt on the
async worker (L9); no-op `auth_password`/`codes_delete` returning success (L8);
`gen_token`-based activation codes → crypto-random (L9, rides B1); webhook +
scheduled-post retry caps (L8); systemd unit extra hardening (L9).

## Architecture notes (L11, accepted into the record)

- Sound: single SQL surface in `Db` (95 `pub fn`, no raw SQL outside it); role
  gating centralized at dispatch head; distribution story shipped; ladder is
  machine-checked.
- Boundary debt: subscription-grant orchestration duplicated at 6 call sites
  (`api.rs:560,895,3425`, `commands.rs:631,692,767`) — extract a core
  `services/` layer (this also fixes B6 structurally). `api.rs` (3,671 lines)
  is an axum interface owned by L8 while its twin `web.rs` is L7 — ownership
  decision below. `WebState` holds a `Bot` — web performs Telegram side
  effects directly; must go through services before v2 worker split.
- Doc drift: README architecture section omits `api.rs`/`web.rs`; two
  ARCHITECTURE.md files (root vs `docs/`) — fold in v1.1 (L4).

## Plan

### v1.1 — security & money correctness (entry: gates green; exit: floor met)

1. **WS1 Security floor (L9→L8→L7, parallel):** B1 CSPRNG + rotation (L9);
   B2 setup nonce/close-on-configure (L8); B3+B4 webhook & claim fail-closed +
   envelope fix (L8); B5 seller/grant scoping (L8); XSS escaping (L7);
   API rate limit + bounded lockout (L8); doctor/install secret hygiene (L9).
2. **WS2 Money correctness:** B6 `claim_and_grant` transaction + UNIQUE index
   + route all three granters through it (L8/L9, M); B7 clamps (S); expiry
   sweeper on a timer (S); seller-scope middleware for web/API/bot channel
   args (M); plan namespace `(seller, name)` (M).
3. **WS3 Product hardening:** `make backup` via SQLite `.backup` (L9);
   backup/restore round-trip test + three-profile smoke in CI (L3); docs
   dedupe (L4); extract inline CSS to the token contract (L6/L6-C).

### v2 — scaling (unchanged; entry: >1k sellers or DB p99 >50ms)

Postgres behind the same `Db` trait; bot/web worker split enabled by the
services seam; `spawn_blocking`/pooling decision revisited with measurements.

## Decisions pending owner approval (T1 — proposals, not yet accepted)

1. **api.rs ownership** → *recommend:* move to L7 after services extraction.
2. **Services layer timing** → *recommend:* v1.1 (fixes B6 structurally).
3. **Backup as product feature** → *recommend:* yes, v1.1.
4. **Connection pooling now?** → *recommend:* no; keep `Mutex<Connection>`,
   document; revisit with latency data.
5. **dialogue.db stays separate in v1** → *recommend:* yes; document in the
   data model (currently unstated).

## Sign-off matrix

| Rung | Role in this review | Status |
|------|--------------------|--------|
| L11 | Review + plan + decisions | ✔ delivered (this document) |
| L10 | Blocking findings B1–B5 + HIGH set | ✔ delivered; floor gates WS1 |
| L8 | P0/P1 health + feasibility sizing | ✔ delivered; owns most WS1/WS2 |
| L3 | Citation verification + gate snapshot | ✔ verified (5/5 spot-checks exact) |

Per `AUTO_ROUTE.md` §2: B1–B7 remediation is T0/T1 implementation work owned
by the named rungs; anything touching real payment value during fixes is T2.

---

## Remediation record (2026-10-25, same day)

**Status: WS1 + WS2 + WS3(core) IMPLEMENTED and gated.** Dispatched as three
parallel rung-owned agents (L9: util/db/scripts; L8: main/commands/api;
L7: web), integrated by the orchestrator, verified by the L3 suite
(fmt / clippy / **68 tests** / audit / deny / routing contract) plus live
smoke tests.

| Finding | Status | Where |
|---------|--------|-------|
| B1 forgeable tokens | ✅ fixed | `util.rs` CSPRNG (`getrandom`); one-time migration clears all sessions + API keys; `generate_code` now CSPRNG |
| B2 setup replay | ✅ fixed + **smoke-proven** | DB `setup_done` gates `/setup`; after POST, `/setup` → 303 without restart |
| B3 unsigned webhook | ✅ fixed | signature header mandatory, constant-time compare, 503 when unconfigured |
| B4 fail-open claim | ✅ fixed | claim → verify (robust `items`/`result.items`/array parsing) → grant-if-new → finalize; fail-closed on every error path; user binding (None ⇒ admin/owner only) |
| B5 auto-seller + `/grant` reach | ✅ fixed | unconditional `upsert_seller` removed; sellers resolve only their own channel; global fallback admin/owner only |
| B6 double/missed grant | ✅ fixed | `claim_invoice_for_grant` conditional UPDATE serializes granters; 'granting' re-queues crash recovery; unique index attempted (warn+skip on legacy dupes) |
| B7 overflow panic | ✅ fixed | `MAX_PLAN_DAYS=3650` clamp; `try_days` + `checked_add_signed` (chrono's `Duration::days` itself panics — caught in review of the fix) |
| HIGH owner password default | ✅ fixed | unset `WEB_PASSWORD` → CSPRNG password printed once, never the owner id |
| HIGH stored XSS | ✅ fixed | `esc()` on 13 dynamic interpolation sites |
| HIGH cross-seller writes | ✅ fixed (web/api/bot) | `channel_of(seller)` scoping across ~17 call sites; `/redeem` resolves the code's own seller |
| HIGH self-marked paid | ✅ fixed | manual `paid` transitions rejected (400) |
| HIGH login rate limit | ✅ fixed | shared `web::login_throttle`/`login_ok` used by BOTH web and API logins; bounded map |
| MED doctor/install hygiene | ✅ fixed | no shell-sourcing of `.env`; token never in argv (`curl -K`); installer never prints env values; proxy ⇒ `WEB_BIND=127.0.0.1` unconditional |
| WS3 backup | ✅ shipped | `scripts/backup.sh` (WAL-safe `sqlite3 .backup`), `make backup`; tested |
| Expiry sweeper | ✅ shipped | 6h background job shared with `/expirecheck` |

**Known deferred** (recorded, not lost): seller-scope middleware for the
remaining `/edit`-family bot commands relies on Telegram-side rights (B5
partial); plan namespace `(seller, name)` global-lookup fix (P1) pending;
`Mutex<Connection>` pooling decision (v2, decision 4); api.rs ownership move
to L7 (decision 1, after services extraction); docs dedupe (L4).
