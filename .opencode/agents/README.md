# Agents for Telegram Business Platform

Subagents available for this project, dispatched via OpenCode. The ladder and
routing rules live in `AUTO_ROUTE.md`; the methodology in the `level-router`
skill; the contract is machine-checked by `python3 scripts/verify-agent-tiers.py`.

| Agent | Level | Ask | Territory |
|-------|-------|-----|-----------|
| associate-verification | L3 | "Is this actually true?" | read-only: quality gates, tests, verification |
| junior-media-writer | L4 | "Write the docs / release note." | `docs/**`, `*.md` |
| senior-finance | L5 | "What should this cost? What is the user owed?" | `docs/CRYPTO_PAY.md` (spec only; L8 writes code) |
| senior-design-ux | L6 | "Make this look and behave like one product." | `design-system/**` (spec; L7 implements) |
| chief-design-officer | L6-C (charter) | "Does this honor the token contract?" | `DESIGN.md` only |
| staff-client-platforms | L7 | "Implement it on the web/bot surface." | `src/web.rs`, `src/handlers/callbacks.rs`, `design-system/**` |
| staff-server | L8 | "Build the bot flow / endpoint / migration." | `src/handlers/**`, `src/main.rs`, `src/ai/**`, `src/dialogue.rs`, `src/roles.rs`, `src/api.rs`, `schema.sql` |
| staff-core-systems | L9 | "Change the data layer, crypto, config, or build." | `src/db.rs`, `src/crypto.rs`, `src/config.rs`, `src/util.rs`, `Cargo.toml`, `vendor/**`, `Dockerfile`, `deny.toml` |
| principal-security | L10 | "Is this safe? Is this a leak?" | `docs/SECURITY.md`; blocks, never remediates |
| fellow-architect | L11 | "What should the interface between domains be?" | `ARCHITECTURE.md`, `AGENT.md`, boundaries |

Rules that the whole ladder shares:

- L3, L5, L10 have **no writable code territory** by design (verify / decide /
  block). A task needing them to edit needs a second rung, not a broader grant.
- Every agent denies `rm -rf`, `sudo`, `git push`, `git reset --hard`, and
  reading `*.env`. No agent may edit `.opencode/**` or `scripts/**` — the
  machinery that binds the ladder is human/orchestrator territory.
- Mandatory: run L3 verification before reporting any implementation complete.
