# Product Plan — Telegram Business Platform

## Vision
A **general-purpose, multi-tenant Telegram business platform** that any seller,
creator, or business can use to sell access, products, and services — managed
from Telegram and a web dashboard, deployable anywhere.

## Where we are (2026-10-25) — v1.2.1 shipped

**Business types covered:** channel subscriptions/memberships ✅ · activation
codes ✅ · one-time digital products (link/file/text, auto-delivered) ✅ ·
physical products with order tracking ✅ · refunds/payouts with owner-approved
Crypto Pay transfers ✅ · **bookings = the remaining type (v2)**.

**Platform:** single Rust binary (lib + bin) — desktop (.exe/.app/tar.gz,
release CI for win/mac/linux x64+arm64), existing-server installer with
hardened systemd unit, Docker, subpath mounting (`WEB_BASE_PATH`), embeddable
(`embed::AgentAppBuilder`), compile-time command registry for extensions.

**Engineering:** role-level architecture (L3–L11) machine-checked by
`scripts/verify-agent-tiers.py`; security floor remediated same-day (CSPRNG,
atomic + fail-closed payment claims, setup gating, XSS, throttling); 94 tests
incl. property invariants + libFuzzer; WAL-safe `make backup`; `make doctor`
diagnostics; full gate suite in `make test`.

Docs: `ARCHITECTURE.md` (system map), `AGENT.md` (handbook + decision log),
`AUTO_ROUTE.md` (routing), `docs/ARCHITECTURE-SCALE-PLAN.md` (fullness audit,
gap designs, scale seam contracts), `docs/DEPLOY_PROFILES.md`, `docs/DESKTOP.md`.

## Next milestones

### Product roadmap (from product review, 2026-10-25 — adoption-ranked)
1. **Front door (blocking):** push to GitHub + tag; README screenshots, 60-second
   intro, demo. A product without a front door is a private project.
2. **Owner approvals in Telegram (the one next feature):** `/refundapprove`,
   `/payoutapprove` bot commands (owner/admin-gated) — money decisions belong
   where the owner lives, on their phone, not only in the web API.
3. **Empty-state onboarding:** first-login checklist (create first product →
   test-buy → verify delivery); measure time-to-first-sale (<10 min target).
4. **Trust pack:** public security summary, changelog automation, versioned
   releases; external pentest before significant real money.
5. **Seller analytics:** revenue/subscriber/churn views in the dashboard
   (data exists via API; surface it).
6. **i18n:** Telegram commerce is heavily non-English (RU/FA/ES/ID first).

### v1.3.x (polish, on demand)
- `/buy seller/name` disambiguation syntax (S).
- Metrics/tracing spans for the performance budgets (observability) (S/M).
- Remove legacy `/process` refund endpoints (S). Changelog automation (S).
- First real-world pilot: one bot, one channel, one digital + one physical
  product, one refund, backup-restore drill.

### v2 (trigger-driven)
- Bookings: services/slots/bookings reusing the invoice pipeline.
- Scale seams — **only on trigger** (>1k active sellers or DB p99 > 50ms):
  `Db` trait extraction (Postgres-ready), services layer expansion, worker
  split, connection pooling. Contracts pre-documented in the scale plan.

### Owner decisions (business, not code)
1. Go-live with real transfers (T2 — small test amount first).
2. Code-signing certificates (SmartScreen/Gatekeeper).
3. Hosted/SaaS offering for non-technical sellers.

## Success metrics (unchanged)
- Time to first sale < 10 minutes · zero-config onboarding · 99.9% uptime
  (hosted version).
