# REST API Reference

Base URL: `https://<host>/api/v1`. Health check: `GET /health` (no auth).

## Auth
All endpoints except `/health`, `/api/v1/auth/login`, `/api/v1/webhooks/cryptopay` require authentication via `session` cookie or `Authorization: Bearer <token>`.

Roles: `owner > admin > seller > user`. Sellers are auto-scoped to their own `seller_id`.

### Authentication
| Method | Path | Role | Description |
|--------|------|------|-------------|
| POST | /auth/login | public | `{username, password}` → `{token, expires_at, account_id, role, seller_id}` |
| POST | /auth/logout | any | Revoke session |
| GET | /auth/me | any | Current account |
| POST | /auth/password | any | `{new_password}` |
| GET | /auth/api-keys | seller+ | List API keys |
| POST | /auth/api-keys | seller+ | `{name, scopes}` → returns full key once |
| DELETE | /auth/api-keys/{id} | seller+/owner | Revoke key |

### Sellers
| Method | Path | Role | Description |
|--------|------|------|-------------|
| GET | /sellers | admin+ | List sellers |
| GET | /sellers/me | seller+ | Current seller |
| GET | /sellers/{user_id} | self/admin+ | Seller detail |
| POST | /sellers | admin+/self | Upsert seller |
| POST | /sellers/{user_id}/channel | self/admin+ | Set default channel |
| POST | /sellers/{user_id}/ai-settings | self/admin+ | Set AI settings |

### Plans
| Method | Path | Role | Description |
|--------|------|------|-------------|
| GET | /plans | scoped | List plans |
| POST | /plans | seller+ | Create plan |
| GET | /plans/{id} | scoped | Plan detail |
| PATCH | /plans/{id} | owner/admin/owner-plan | Update plan |
| DELETE | /plans/{id} | owner/admin/owner-plan | Delete plan |
| POST | /plans/{id}/active | owner/admin/owner-plan | `{active}` |

### Subscriptions
| Method | Path | Role | Description |
|--------|------|------|-------------|
| GET | /subscriptions | scoped | List subscriptions |
| POST | /subscriptions | seller+ | Grant subscription |
| GET | /subscriptions/{id} | scoped | Subscription detail |
| PATCH | /subscriptions/{id} | seller+ | Expire/revoke/extend |
| POST | /subscriptions/expire-check | seller+ | Revoke expired |
| GET | /users/{user_id}/subscriptions | self/admin+ | User's subscriptions |

### Invoices
| Method | Path | Role | Description |
|--------|------|------|-------------|
| GET | /invoices | scoped | List invoices |
| POST | /invoices | seller+ | Create invoice |
| GET | /invoices/{invoice_id} | scoped | Invoice detail |
| POST | /invoices/{invoice_id}/status | seller+ | `{status}` |
| POST | /invoices/{invoice_id}/claim | scoped | Verify payment + grant access |
| POST | /webhooks/cryptopay | public (signed) | CryptoPay webhook |

### Codes
| Method | Path | Role | Description |
|--------|------|------|-------------|
| GET | /codes | scoped | List codes |
| POST | /codes | seller+ | Generate codes |
| GET | /codes/{code} | scoped | Code detail |
| DELETE | /codes/{code} | scoped | Delete code |
| GET | /codes/{code}/redemptions | scoped | Redemptions |
| POST | /codes/{code}/redeem | auth | Redeem code |

### Posts
| Method | Path | Role | Description |
|--------|------|------|-------------|
| GET | /posts | scoped | List posts |
| POST | /posts | seller+ | Send + log post |
| GET | /posts/{id} | scoped | Post detail |
| PATCH | /posts/{id} | seller+ | Edit post |
| DELETE | /posts/{id} | seller+ | Delete post |
| POST | /posts/{id}/pin | seller+ | Pin message |
| POST | /posts/{id}/unpin | seller+ | Unpin message |
| POST | /broadcasts | seller+ | `{text}` scoped broadcast |

### Users
| Method | Path | Role | Description |
|--------|------|------|-------------|
| GET | /users | scoped | List users |
| GET | /users/{user_id} | self/admin+ | User detail |
| POST | /users/upsert | seller+ | Upsert user |

### Stats & Config
| Method | Path | Role | Description |
|--------|------|------|-------------|
| GET | /dashboard/summary | scoped | KPIs |
| GET | /revenue | scoped | Revenue total |
| GET | /channels | scoped | Known channels |
| GET | /channels/{id} | scoped | Channel info |
| PATCH | /channels/{id} | seller+ | Update channel |
| GET | /config/channel | auth | Default channel |
| POST | /config/channel | seller+ | Set channel |

### AI
| Method | Path | Role | Description |
|--------|------|------|-------------|
| GET | /ai/templates | seller+ | List templates |
| POST | /ai/templates | seller+ | Create template |
| GET | /ai/templates/{name} | seller+ | Template detail |
| PATCH | /ai/templates/{name} | seller+ | Update template |
| DELETE | /ai/templates/{name} | seller+ | Delete template |
| GET | /ai/usage | seller+ | Usage summary |

### Finance
| Method | Path | Role | Description |
|--------|------|------|-------------|
| GET | /finance/summary | scoped | Revenue summary |
| GET | /finance/revenue-by-seller | admin+ | Revenue by seller |
| GET | /finance/payouts | scoped | List payouts |
| POST | /finance/payouts | admin+ | Create payout |
| PATCH | /finance/payouts/{id} | admin+ | Update payout |
| GET | /finance/refunds | scoped | List refunds |
| POST | /finance/refunds | seller+ | Create refund |
| GET | /finance/export | scoped | CSV export |

### Admin
| Method | Path | Role | Description |
|--------|------|------|-------------|
| GET | /admin/sellers | admin+ | List sellers |
| GET | /admin/sellers/{user_id} | admin+ | Seller detail |
| PATCH | /admin/sellers/{user_id} | admin+ | Update seller |
| GET | /admin/stats/global | admin+ | Global stats |
| GET | /admin/admins | owner | List admins |
| POST | /admin/admins | owner | Add admin |
| DELETE | /admin/admins/{user_id} | owner | Remove admin |
| GET | /admin/web-accounts | owner | List web accounts |
| POST | /admin/web-accounts | owner | Create web account |
| DELETE | /admin/web-accounts/{id} | owner | Delete web account |
| PATCH | /admin/web-accounts/{id}/password | owner | Set password |
| GET | /admin/audit-log | admin+ | Audit log |

## Error format
```json
{ "error": { "code": "not_found", "message": "invoice not found" } }
```

## Pagination
`?limit=50&offset=0` → `{ "items": [...], "total": N }`
---

## Products & Shop (added v1.2)

Auth: session cookie (web) or `Authorization: Bearer <api-key>` where noted; JSON bodies.

| Method & path | Role | Description |
|---|---|---|
| `GET /api/v1/shop` | public | Active products for sale (no delivery payloads). |
| `GET /api/v1/products` | seller (own) / admin (all) | List products incl. inactive. |
| `POST /api/v1/products` | seller | Create: `{name, price, asset, kind: digital_link|digital_file|physical, delivery?, stock?}`. Name collisions (plan or product, same seller) → 400. |
| `DELETE /api/v1/products/{name}` | seller/admin | Deactivate. |
| `POST /api/v1/invoices/{id}/claim` | buyer (invoice owner) / admin | Verify via Crypto Pay (fail-closed) → grant + DM delivery + finalize. Product invoices grant once (UNIQUE); out-of-stock finalizes without grant. |

## Finance (added v1.2)

Money movement is owner-approved (T2) and audited (`audit_log`).

| Method & path | Role | Description |
|---|---|---|
| `POST /api/v1/finance/refunds` | seller | Create refund request from one of their paid invoices (amount/asset default to the invoice's). |
| `POST /api/v1/finance/refunds/{id}/approve` | admin/owner | pending → approved. |
| `POST /api/v1/finance/refunds/{id}/execute` | admin/owner | Executes Crypto Pay `transfer`; on success: executed, invoice refunded, subscription revoked + buyer kicked; failure → 500, stays approved. |
| `POST /api/v1/finance/payouts` | seller | Create payout request `{amount, asset, destination?}`. |
| `POST /api/v1/finance/payouts/{id}/approve` / `{id}/execute` | owner only | Approve, then execute the transfer. |

Manual `"paid"` transitions on invoices are rejected (400) — payment state changes only through the verified webhook/claim paths.
