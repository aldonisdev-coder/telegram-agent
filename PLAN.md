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
- **MCP server (v1.3.x, in progress).** The fourth thin interface over the
  domain core: a hand-rolled MCP (JSON-RPC) endpoint at `POST /api/v1/mcp`
  inside the existing axum router — zero new dependencies, same `WebState`
  (db + cfg + bot). Designed AI-first, because the primary caller is an
  AI agent, not a human:
  - **Auth:** `MCP_OWNER_TOKEN` env (static bearer → Owner principal).
    Empty/unset = endpoint disabled entirely (secure default OFF).
    Constant-time comparison. v2: a `mcp_tokens` table with per-seller
    tokens issued via bot command; `Principal { role, seller_id }` is
    threaded through every tool now so that is additive, not a redesign.
  - **Tool tiers — the permission model:**
    - *Read* (`readOnlyHint`, direct): list_channels, list_plans,
      list_posts, subs_report (user ids masked by default), list_codes
      (codes masked by default).
    - *Write* (direct, audited): publish_post, edit_post.
    - *Sensitive* (`destructiveHint`, **server-side approval gate**):
      gen_codes (returns secrets), delete_post (destructive), broadcast
      (high blast radius). Calling one creates a pending row and returns
      `requires_approval` — nothing executes. The OWNER approves via the
      bot (`/approvals`, `/approve <id>`, `/deny <id>`, owner-only,
      15-minute expiry); execution happens on approval, and secrets
      (generated codes) are delivered to the owner's Telegram chat, never
      back into the AI's transcript.
  - **Audit:** every tool call and every approve/deny lands in
    `mcp_audit` (tool, role, ok, truncated detail).
  - **Rate limit:** reuse the AI rate-limiter pattern on the endpoint.
  - Verification: in-process axum oneshot tests (no network, no real bot) —
    auth, protocol methods, tier annotations, masking, the full
    approval lifecycle, audit rows.

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
