# Command Reference

Role-based access: Owner > Admin > Seller > User. Sellers manage their own data; admins/owner see all.

## Roles
- **User (public)**: `/start`, `/help`, `/menu`, `/plans`, `/buy`, `/paid`, `/redeem`, `/setup`, `/becomeseller`.
- **Seller**: user commands + channel, plans, codes, subscriptions, crypto, broadcast, AI, web dashboard.
- **Admin**: seller commands (global view) + `/sellers`.
- **Owner**: admin commands + `/addadmin`, `/removeadmin`, `/admins`, `/webusers`.

## Web dashboard
- `/dashboard` — show web dashboard URL and credentials (creates account if missing).
- `/setwebpassword <new>` — change web dashboard password.

## General
- `/start` — welcome + menu. (public)
- `/help` — list commands. (public)
- `/menu` — inline menu. (public)

## Admin / broadcasting
- `/broadcast <text>` — send a message to all registered users.

## Channel
- `/channels` — list known channels.
- `/setchannel @chan|id` — set default target channel.
- `/channel` — show default target channel.
- `/info [@channel]` — channel title, description, member count.
- `/post [@channel] <text>` — post text.
- `/postphoto [@channel] [caption]` — then send a photo to post it.
- `/edit [@channel] <msg_id> | <text>` — edit posted message.
- `/delete [@channel] <msg_id>` — delete posted message.
- `/pin [@channel] <msg_id>` — pin message.
- `/unpin [@channel]` — unpin all.
- `/unpinmsg [@channel] <msg_id>` — unpin specific.
- `/invite [@channel]` — create invite link.
- `/settitle [@channel] <text>` — set channel title.
- `/setdesc [@channel] <text>` — set channel description.

## Plans & subscriptions
- `/plans` — list plans.
- `/planadd name | price | asset | days` — add plan.
- `/plandel name` — remove plan.
- `/buy <plan>` — create payment invoice.
- `/paid <invoice_id>` — verify payment and grant access.
- `/grant user_id | days` — manual grant.
- `/revoke user_id` — revoke access.
- `/subs` — list subscriptions.
- `/expirecheck` — revoke expired subscriptions.

## Activation codes
- `/gencodes plan | count | [days]` — generate codes.
- `/codes` — list codes.
- `/redeem CODE` — redeem code.

## Crypto Pay
- `/crypto setup <token>` — save token.
- `/crypto me` — app info.
- `/crypto balance` — balance.
- `/crypto rates` — exchange rates.
- `/crypto invoices [status]` — list invoices.
- `/crypto invoice <amount> <asset> [desc]` — create invoice.
- `/crypto check <amount> <asset>` — create check.

## Bot
- `/setupbot` — configure bot profile/commands.
- `/becomeseller` — onboard as a seller (creates web dashboard account).

## Admin (owner-only)
- `/addadmin <user_id> [username]` — add platform admin.
- `/removeadmin <user_id>` — remove platform admin.
- `/admins` — list platform admins.
- `/sellers` — list sellers (admin/owner).
- `/webusers` — list web accounts (owner).
## Products & shop (v1.2)

- `/productadd name | price | asset | kind | delivery | [stock]` — create a product (seller). `kind`: `digital_link` | `digital_file` | `physical`; `delivery` is the link/text or Telegram `file_id`; stock blank = unlimited.
- `/products` — list your products (seller/admin). Dashboard equivalent: **Products** page.
- `/productdel <name>` — deactivate a product.
- `/shop` — public listing of everything on sale (products + plans).
- `/buy <name>` — buy a product or plan by exact name; returns a Crypto Pay invoice. Ambiguous names (two sellers) are refused — ask the seller.
- Delivery: after verified payment the buyer receives the payload as a private message (document re-sent for `digital_file`).

## Finance (v1.2, owner/admin via API)

- Refunds: create (seller, from a paid invoice) → approve → execute (owner/admin; executes a Crypto Pay transfer, revokes the grant and kicks the buyer; every step audited).
- Payouts: create (seller) → approve → execute (owner only; transfers to destination).
- API surface: `/api/v1/products`, `/api/v1/shop`, `/api/v1/finance/refunds|payouts/{id}/approve|execute`.

## Physical orders (v1.3)

- A `physical` product purchase creates an order automatically after verified payment.
- `/orders` — list orders (seller: own; admin/owner: all).
- `/orderset <order_id> <status> [tracking]` — seller updates status (`pending|shipped|delivered|refunded|cancelled`) and optional tracking number.
