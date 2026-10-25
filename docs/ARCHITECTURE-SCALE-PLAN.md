# Architecture Fullness & Scale Plan — 2026-10-25 (L11)

**Status: T1 proposal — awaiting owner approval. Nothing below is implemented
except where marked "shipped".** Produced by the L11 fellow-architect dispatch;
evidence cited from the tree as of this date. Companion to
[`reviews/2026-10-25-architecture.md`](./reviews/2026-10-25-architecture.md).

## 1. Fullness verdict

| Dimension | Verdict | Evidence |
|---|---|---|
| Product model | **PARTIAL (narrow)** | Only day-counted single-channel subscriptions: `plans.duration_days NOT NULL` (schema.sql:57), single `plans.channel_id` (:58), `subscriptions.expires_at NOT NULL` (:73). No product/order/booking/delivery entity exists. |
| Data layer | FULL for its shape | 98 `pub fn` behind `Db`, parameterized SQL, additive-migration pattern proven (db.rs:120–143). Gaps are missing entities, not missing discipline. |
| Security | FULL (post-remediation) | CSPRNG, atomic `claim_invoice_for_grant` (db.rs:587), fail-closed webhook/claim, setup gating. Four debt items remain (§4 v1.2 items 1–4). |
| Distribution/ops | FULL | 4 deployment profiles, `WEB_BASE_PATH` mounting, `embed::AgentAppBuilder`, WAL-safe backup. |
| Testing | FULL | 96 tests incl. property invariants + libFuzzer + mount integration suite. |
| Observability | PARTIAL | Logs only; no metrics endpoint, §9 budgets not measured (no tracing spans for p99). |
| Scaling readiness | PARTIAL by design | `Mutex<Connection>`, no `spawn_blocking`, no `Db` trait — acceptable per AGENT.md §10, but seams are code facts, not contracts (fixed in §3). |
| Refunds/payouts | **MISSING (spec-only)** | Tables + CRUD exist (schema.sql:160–183, api.rs:1725–2011) but `crypto.rs` has no `transfer` (surface ends at `create_check`, crypto.rs:93); "process" silently flips status (api.rs:1909). No money can move. |

## 2. Gap-closing architecture

### a. Products + digital delivery + lifetime grants (v1.2)

```sql
CREATE TABLE products (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  seller_id INTEGER NOT NULL,
  name TEXT NOT NULL, price TEXT NOT NULL, asset TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('digital_link','digital_file','physical')),
  delivery TEXT,            -- link/text, or Telegram file_id for digital_file
  stock INTEGER,            -- NULL = unlimited
  active INTEGER NOT NULL DEFAULT 1, created_at TEXT NOT NULL,
  UNIQUE(seller_id, name)
);
CREATE TABLE product_grants (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  seller_id INTEGER NOT NULL, user_id INTEGER NOT NULL,
  product_id INTEGER NOT NULL, invoice_id TEXT NOT NULL UNIQUE,
  status TEXT NOT NULL DEFAULT 'active', created_at TEXT NOT NULL
);
ALTER TABLE invoices ADD COLUMN product_id INTEGER;  -- additive only
```

- **Reuse the invoice pipeline** (create → claim → verify → grant → finalize
  is item-agnostic); nullable `invoices.product_id` selects the grant branch.
  Existing rows (NULL) take the plan path — behavior byte-identical.
- **Separate `product_grants` table** — `subscriptions.expires_at NOT NULL`
  cannot be relaxed in SQLite without a table rebuild; a dedicated table makes
  lifetime structural (no expiry column), keeps every existing expiry
  predicate safe by construction, and inherits the B6 double-grant fix via
  `UNIQUE(invoice_id)`.
- **Delivery**: after verified claim, `services::deliver` DMs the payload —
  link/text or Telegram file_id re-send. Never rendered in group chats.
- **`/buy` multiplexes** products and plans via `resolve_purchasable()`;
  creation-time cross-table name-collision rejection.

### b. Orders-lite (v1.3, preview)

`orders(order_id TEXT UNIQUE, seller_id, product_id, qty, buyer_user_id,
invoice_id UNIQUE, ship_* fields, status pending→paid→shipped→delivered|
refunded, tracking)`; shipping collected via the dialogue wizard; index on
`(seller_id, status)`.

### c. Bookings (v2, preview)

`services`, `slots(starts_at, capacity, booked)`, `bookings(held→paid→
confirmed|cancelled)` reusing the invoices pipeline with the generalized item
reference; background job expires stale `held` slots.

### d. Refunds/payouts real flows (v1.2, prerequisite for a)

| Flow | States | Owner |
|---|---|---|
| refund | pending → approved → executed \| rejected | L5 spec · L8 code · L10 audit |
| payout | pending → approved → paid | same |

Execution = new `crypto.rs::transfer` (Crypto Pay transfer API), receipt
stored; refund-executed also revokes the grant (status `refunded` + channel
kick). Every transition writes `audit_log`. The current silent status flip
becomes a flagged manual fallback. First real-value transfers are T2
(human-in-loop) per AUTO_ROUTE §2.

## 3. Scale seam contracts (documented; built ONLY on trigger: >1k sellers or DB p99 >50ms)

1. **`Db` trait** — mechanical extraction of the existing 98-method surface;
   `SqliteDb` impl; call sites unchanged. Enables Postgres without touching
   handlers.
2. **`services/` layer** — `grant.rs` (absorbs the 6 duplicated grant sites),
   `deliver.rs` (v1.2 needs it anyway), `expiry.rs` (existing sweeper); moves
   `WebState`'s direct `Bot` handle behind services — prerequisite for any
   worker split.
3. **`spawn_blocking`/pool** — keep `Mutex<Connection>` until a measured p99
   breach; v1.2 adds tracing spans so the budget becomes measurable.

## 4. Sequenced roadmap

**v1.2 — debt, money, products** (exit: a digital product sells, delivers,
and refunds end-to-end; all four debt items closed):

| # | Item | Size | Rung |
|---|------|------|------|
| 1 | Debt: `.env` rewrite preserves unknown keys | S | L9 |
| 2 | Debt: `/edit`-family seller-scope checks | M | L8 |
| 3 | Debt: plan lookup `(seller,name)`, retire `get_plan_global` | S | L9 |
| 4 | Debt: docs dedupe (fold docs/ARCHITECTURE.md into root) | S | L4 |
| 5 | Refunds/payouts real flows (prereq for 6) | M | L5/L8/L10 |
| 6 | Products + grants + delivery + `services::deliver` | L | L9/L8/L7 |
| 7 | Tests: product lifecycle property (claim→grant→deliver once) + debt regressions | M | L3 |

**v1.3 — orders-lite** (entry: v1.2 exit). **v2 — bookings + seams if
triggered** (entry: v1.3 exit or scale trigger).

## 5. Decisions awaiting owner approval (T1)

1. **digital_file storage** → *recommend:* Telegram `file_id` passthrough
   (Telegram hosts the blob; no new key management/backup surface). Risk:
   delivery depends on Telegram availability.
2. **Lifetime representation** → *recommend:* structural (grants table has no
   expiry column); sentinel dates leak into UI and arithmetic.
3. **`/buy` multiplex vs `/product` command** → *recommend:* multiplexed
   `/buy` + unified `/shop`; one mental model; guarded by lifecycle tests.
4. **Shipping-address privacy** → *recommend:* full address owner-only,
   seller sees city/country, every full read audited.
5. **Separate `product_grants` table** → *recommend:* yes (additive-only,
   reuses invoice-uniqueness fix).

All designs stay single-process, single-binary, SQLite (AGENT.md §10). No
seam is built ahead of its measured trigger.

---

## Implementation record — v1.2 (2026-10-25, same day)

**Status: IMPLEMENTED and gated**, per the approved recommendations (1–5
taken as recommended). Dispatched L9 (schema/db/crypto-transfer), L8
(services/deliver, bot commands, API money flows), L7 (web env-fix +
products page); orchestrator integrated (tuple-layout fixes, clippy), L3
gated. Gates: fmt ✓, clippy -D warnings ✓, **93 lib tests** (incl. new
product/grant/stock/refund/payout unit tests + the
`prop_product_grant_exactly_once` lifecycle property) + 9 mount tests ✓,
routing verifier ✓, release repackaged, live smoke: `/api/v1/shop` 200 on a
fresh instance.

| Item | Status |
|---|---|
| 1 .env rewrite preserves unknown keys | ✅ web.rs (reads existing file, preserves non-form keys verbatim) |
| 2 /edit-family seller scope | ✅ (main.rs/api.rs claim paths seller-scoped; commands converted) |
| 3 plan lookup (seller,name) | ✅ get_plan_global callers retired across bot/api; find_plans_by_name for buyers |
| 4 docs dedupe | ✅ docs/ARCHITECTURE.md → pointer |
| 5 refunds/payouts flows | ✅ state machines + crypto `transfer()` + owner-approved execution + audit_log + refund revokes grant & kicks |
| 6 products + delivery | ✅ products/product_grants/migration; /productadd,/products,/productdel,/shop; /buy multiplex; claim branches (bg job, /paid, API) grant→deliver(DM)→finalize; stock atomic; dashboard /products page |
| 7 tests | ✅ +6 unit + 1 property test |

Deferred to v1.3 as planned: orders-lite (physical fulfillment wizard);
`send_document` re-send for `digital_file` (text DM with file link/id used
in v1.2); seller-disambiguation syntax for ambiguous `/buy` names.
