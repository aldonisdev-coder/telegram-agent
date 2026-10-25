# Crypto Pay Integration

## Setup
- Create app in @CryptoBot → Crypto Pay.
- Set token via `/crypto setup <token>` or `CRYPTO_PAY_TOKEN` env.

## Payment flow
1. `/buy <plan>` creates invoice via `createInvoice`.
2. Bot stores invoice with plan_id and user_id.
3. User pays using pay_url.
4. `/paid <invoice_id>` calls `getInvoices` to verify status.
5. On `paid`, subscription created and invite link sent.

## Methods used
- `getMe`, `getBalance`, `getExchangeRates`
- `createInvoice`, `getInvoices`
- `createCheck`

## Notes
- Always verify payment server-side via API before granting access.
- Invoice status may lag; retry `/paid` if needed.