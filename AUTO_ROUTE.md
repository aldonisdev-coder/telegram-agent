# AUTO_ROUTE.md — Role-Level Task Router

> **Single source of truth for "who does what" in this repository.**
> Load the `level-router` skill at the start of every task, then use this file
> to route. The contract below is **machine-checked** — run
> `python3 scripts/verify-agent-tiers.py` after touching any of it.

Routing any task takes three steps:

1. **Pick the rung** (§1) — which role owns this kind of change.
2. **Pick the authority tier** (§2) — T0 act, T1 propose, T2 escalate.
3. **Dispatch** (§3–§4) — single rung or composite, in parallel where independent.

## 0. Operating principles

1. **Read before write.** Never propose changes to code you haven't read.
2. **Preserve multi-tenancy.** Every business entity carries `seller_id`;
   superadmin (`OWNER_ID`) has global visibility.
3. **Role-gated runtime.** Commands/handlers check `roles::role_of()`
   (Owner > Admin > Seller > User) before acting.
4. **Quality gates before merge** (§7): build, test, clippy, fmt, audit.
5. **Use installed skills when relevant** (§6).
6. **Parallel sub-agents** for independent work; serialize dependent steps.
7. **No secrets in code/logs; SQL parameterized; env-based config.**

## 1. The ladder (L3–L11)

A level is a **blast-radius contract** — how far a role's changes reach and what
it may decide alone — not a title. It is enforced by the permission maps in
`.opencode/agents/*.md` and checked by `scripts/verify-agent-tiers.py`.

| Level | Agent | Ask | Owns |
|-------|-------|-----|------|
| L3 | associate-verification | "Is this actually true?" | Quality gates, tests, `doctor.sh` runs, verification reports — read-only |
| L4 | junior-media-writer | "Write the docs / release note." | `docs/**`, `README.md`, bundle QUICKSTARTs |
| L5 | senior-finance | "What should this cost? What is the user owed?" | Pricing & billing spec — `docs/CRYPTO_PAY.md` |
| L6 | senior-design-ux | "Make this look and behave like one product." | UX spec, a11y floors — `design-system/**` |
| L6-C | chief-design-officer | "Does this honor the token contract and brand?" | Charter: `DESIGN.md` only — specifies, never rewrites client trees |
| L7 | staff-client-platforms | "Implement it on the web/bot surface." | `src/web.rs`, `src/handlers/callbacks.rs`, `design-system/**` |
| L8 | staff-server | "Build the bot flow / endpoint / migration." | `src/handlers/**`, `src/main.rs`, `src/ai/**`, `src/dialogue.rs`, `src/roles.rs`, `src/api.rs`, `schema.sql` |
| L9 | staff-core-systems | "Change the data layer, crypto, config, or build." | `src/db.rs`, `src/crypto.rs`, `src/config.rs`, `src/util.rs`, `Cargo.toml`, `vendor/**`, `Dockerfile`, `deny.toml`, `Makefile`, `scripts/**` (package/install/doctor/icons), release CI |
| L10 | principal-security | "Is this safe? Is this a leak?" | Security review, `docs/SECURITY.md` — blocks; never remediates itself |
| L11 | fellow-architect | "What should the interface between domains be?" | `ARCHITECTURE.md`, `AGENT.md`, module boundaries |

Three rungs have **no writable code territory** and it is not a gap:
**L3** verifies (every mutation denied — it files defects, never fixes them),
**L5** decides the number (L8 writes it), **L10** blocks (the owning rung
remediates). If a task needs one of them to also edit, the task needs a second
rung, not a broader grant.

## 2. Authority tiers

| Tier | Means | Gate |
|------|-------|------|
| **T0 Execute** | Acts and reports. Local, reversible. | Verify locally before reporting. |
| **T1 Propose** | Writes a spec, then waits. | RFC or explicit human approval. |
| **T2 Escalate** | Prepares only. | Human-in-the-loop. Never self-approve. |

**T2 always escalates**, for every rung: destructive ops (`rm -rf`,
`git reset --hard`, force push), production deploys, credentials/keys/signing
material, anything that moves real value (payments, redemption), external
publishing, data deletion. **No rung closes its own T2. Not L11.**

## 3. Quick routing table — task type → dispatch

| Task type | Dispatch (rungs, in order) | Skill to activate |
|-----------|---------------------------|-------------------|
| New feature, multi-file | L11 plan → L8 and/or L7 implement → L3 verify | `brainstorming`, `test-driven-development` |
| Architecture / module boundaries | L11 | — |
| Security review / auth / payments audit | L10 review → owning rung remediates | `code-security-auditor` |
| DB schema / migration / storage | L9 | `rust`, `test-driven-development` |
| Bot command / handler / dialogue flow | L8 | `rust`, `test-driven-development` |
| Web UI / dashboard / templates | L6 spec → L7 implement | `ui-ux-pro-max`, `ui-styling`, `design` |
| Design system / tokens / brand | L6-C specify → L7 implement | `design`, `design-system`, `ui-ux-pro-max` |
| Pricing / plans / billing rules | L5 spec → L8 implement → L10 review | `rust` |
| Tests / QA / smoke tests | L3 | `test-fixing`, `test-driven-development` |
| Docs / README / release notes | L4 | — |
| Rust code (any) | owning rung | `rust` |
| Deploy / install / ops on a server | L9 (`scripts/install.sh`, `Makefile`) → L3 (`doctor.sh`) | — |
| Product/feature scoping | L11 | `brainstorming` |

## 4. Composite dispatch

| Task shape | Dispatch |
|------------|----------|
| Bot feature (command + data) | L8 + L9 (schema) → L3 verify |
| Web dashboard feature | L6 spec + L7 implement (+ L6-C if tokens change) → L3 verify |
| Anything touching money | L5 (the number) + L8 (the code) + L10 (the review) → L3 verify |
| Anything touching auth/secrets | L10 + owning rung → L3 verify |
| Cross-cutting refactor | L11 plan → L8/L9/L7 per tree → L3 verify |
| Ship to production | L3 verify → L10 sign-off → **human approval** |

**Mandatory: run L3 before reporting any implementation complete.** A role
reporting its own work as verified is not verification.

## 5. Conflict lattice

The ladder has floors, not a total order:

1. **L10 security is a floor, not a peer.** No design decision overrides a
   security finding. The remediation is negotiable; the floor is not.
2. **WCAG AA accessibility is a release floor, waivable by nobody — including
   L10.** If a11y and security genuinely cannot be reconciled, the feature
   ships blocked and escalates.
3. **L5 wins on anything that moves money.** L5 owns the number and its
   rounding; L6 owns how it reads. If rounding changes what a user is owed,
   L5 wins and the design adapts.

Accessibility splits by cause, not by surface: contrast, focus, legibility,
screen-reader semantics → L6. Any remediation that would expose a secret or
reveal user presence → L10 rules on what may be surfaced; L6 rules on how it
reads.

**L6-C** routes alongside L6 on any cross-surface coherence work: dispatch it
as a second opinion on the token contract, never as the editor.

## 6. Installed skills (`.opencode/skills/`, project-local)

- **`level-router`** — load first, at the start of every task, before any
  domain work. It is the methodology this file implements.
- `rust` — Rust engineering standard. Activate for any Rust write/review/refactor.
- `ui-ux-pro-max` — design intelligence. Activate for any UI/design work.
- `test-driven-development` — activate before implementing any feature/bugfix.
- `code-security-auditor` — activate before merging security-sensitive changes.
- Also installed: `brainstorming`, `test-fixing`, `staff-engineer-review`,
  `git-pushing`, `review-implementing`, `webapp-testing`, `playwright-skill`,
  `mcp-builder`, `changelog-generator`, `using-git-worktrees`,
  `finishing-a-development-branch`, `banner-design`, `brand`, `design`,
  `design-system`, `slides`, `ui-styling`.

Activation: `/skill-name` or inferred suggestion. On name collision, run as
`/skill:<name>`.

## 7. Quality gates (must pass before merge)

```bash
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt --check
cargo audit          # supply chain
cargo deny check     # licenses
python3 scripts/verify-agent-tiers.py   # routing contract
```

- No `unwrap()` in production paths (use `?` or match).
- All DB writes through `Db` methods; handlers check role before acting.
- Scope all business data by `seller_id`.
- Log via `tracing`; never log secrets.

## 8. Sub-agent dispatch rules

- Launch parallel sub-agents (one per area) in a single message when areas are
  independent; serialize dependent steps.
- Give each sub-agent a complete, self-contained prompt: exact file paths,
  expected output, and the constraints above that apply.
- If a sub-agent fails or runs out of budget, **re-dispatch smaller — do not
  absorb the work yourself.** An orchestrator writing another rung's tree
  bypasses permissions that were correctly configured all along.
- Route every edit to its owner: `src/db.rs`→L9, `src/handlers/**`→L8,
  `src/web.rs`→L7, `design-system/**`→L6, `docs/**`→L4.
- Before dispatching: does the artifact exist? Is the work already done
  (`git status`)? Did you name the exact paths in the brief?

## 9. Escalation

- Cross-cutting / architectural → **L11** fellow-architect.
- Security / payment / secret concern → **L10** principal-security.
- Money movement or pricing ambiguity → **L5** senior-finance.
- Data integrity / scaling / build → **L9** staff-core-systems.
- Anything in §2's T2 list → **human**, always.

## 10. Non-goals (v1)

- No Kubernetes, microservices, or ML.
- No real-time chat beyond Telegram.
- Connection pooling deferred to v2 (WAL + busy_timeout for v1).
