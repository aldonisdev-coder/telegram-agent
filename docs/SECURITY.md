# Security Model

## Authentication
- Telegram: owner/admin gating via OWNER_ID + admins table.
- Web: password (WEB_PASSWORD) + SHA-256 session cookie.

## Authorization
- Public commands: start, help, menu, plans, buy, paid, redeem, setup, becomeseller.
- Admin commands: require `is_admin`.

## Payments
- Always verify payment via Crypto Pay API before granting access.
- Never trust client-side payment claims.

## Secrets
- All tokens in `.env` (gitignored).
- No secrets in code or logs.

## Data
- SQLite file permissions; back up regularly.
- Least-privilege bot admin rights.
---

## Security guarantees as of v1.2.1 (2026-10-25 remediation)

- **Tokens:** all sessions/API keys/codes generated from the OS CSPRNG (`getrandom`); the previous clock-seeded LCG was removed and a one-time migration invalidated every credential issued before the fix.
- **Payments fail closed:** no grant without an explicit `status=="paid"` from the Crypto Pay API; verification errors or a missing token never grant. Webhook signatures are mandatory (constant-time compare).
- **Exactly-once grants:** invoice claiming is an atomic conditional UPDATE; `UNIQUE(invoice_id)` on subscriptions and product grants makes double-delivery structurally impossible (property-tested, fuzzed).
- **Setup is one-shot:** first-run `/setup` closes against a DB flag the moment configuration completes — no replay window.
- **Multi-tenancy:** seller scoping on plan/product/order/finance paths; seller channel fallbacks can no longer reach the global default channel; manual "paid" transitions rejected.
- **Credentials hygiene:** `.env` mode 600, secrets never logged, never in process argv (doctor uses `curl -K`), installer never prints env values.
- **Rate limiting:** shared web+API login throttle (5 failures / 15 min, bounded memory).
- **Money movement:** refunds/payouts require owner approval before any transfer (human-in-the-loop); every transition audited.
- Review + remediation record: `docs/reviews/2026-10-25-architecture.md`; independent penetration testing is recommended before high-volume operation.
