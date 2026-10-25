-- Sellers (multi-tenant)
CREATE TABLE IF NOT EXISTS sellers (
    user_id        INTEGER PRIMARY KEY,
    username       TEXT,
    default_channel TEXT,
    created_at     TEXT NOT NULL
);

-- Target channel configuration (global default)
CREATE TABLE IF NOT EXISTS config (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Posted messages log
CREATE TABLE IF NOT EXISTS posts (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    seller_id   INTEGER NOT NULL,
    chat_id     TEXT NOT NULL,
    message_id  INTEGER NOT NULL,
    kind        TEXT NOT NULL,
    content     TEXT,
    created_at  TEXT NOT NULL
);

-- Crypto Pay invoices
CREATE TABLE IF NOT EXISTS invoices (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    invoice_id   TEXT NOT NULL UNIQUE,
    seller_id    INTEGER NOT NULL,
    amount       TEXT NOT NULL,
    asset        TEXT NOT NULL,
    description  TEXT,
    status       TEXT NOT NULL DEFAULT 'active',
    pay_url      TEXT,
    plan_id      TEXT,
    user_id      INTEGER,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);

-- Admin allow-list (superadmin always admin)
CREATE TABLE IF NOT EXISTS admins (
    user_id    INTEGER PRIMARY KEY,
    username   TEXT,
    added_at   TEXT NOT NULL
);

-- Subscription plans (per seller)
CREATE TABLE IF NOT EXISTS plans (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    seller_id     INTEGER NOT NULL,
    name          TEXT NOT NULL,
    price         TEXT NOT NULL,
    asset         TEXT NOT NULL,
    duration_days INTEGER NOT NULL,
    channel_id    TEXT,
    active        INTEGER NOT NULL DEFAULT 1,
    created_at    TEXT NOT NULL,
    UNIQUE(seller_id, name)
);

-- Subscriptions / access grants
CREATE TABLE IF NOT EXISTS subscriptions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    seller_id   INTEGER NOT NULL,
    user_id     INTEGER NOT NULL,
    plan_id     INTEGER,
    channel_id  TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'active',
    starts_at   TEXT NOT NULL,
    expires_at  TEXT NOT NULL,
    invoice_id  TEXT,
    code_id     INTEGER,
    created_at  TEXT NOT NULL
);

-- Digital products (per seller; /buy multiplexes these with plans by name)
CREATE TABLE IF NOT EXISTS products (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    seller_id   INTEGER NOT NULL,
    name        TEXT NOT NULL,
    price       TEXT NOT NULL,
    asset       TEXT NOT NULL,
    kind        TEXT NOT NULL CHECK (kind IN ('digital_link','digital_file','physical')),
    delivery    TEXT,
    stock       INTEGER,
    active      INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL,
    UNIQUE(seller_id, name)
);

-- Lifetime product grants (one grant per invoice)
CREATE TABLE IF NOT EXISTS product_grants (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    seller_id   INTEGER NOT NULL,
    user_id     INTEGER NOT NULL,
    product_id  INTEGER NOT NULL,
    invoice_id  TEXT NOT NULL UNIQUE,
    status      TEXT NOT NULL DEFAULT 'active',
    created_at  TEXT NOT NULL
);

-- Orders for physical products (one per invoice; v1.3-lite)
CREATE TABLE IF NOT EXISTS orders (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    order_id       TEXT NOT NULL UNIQUE,
    seller_id      INTEGER NOT NULL,
    product_id     INTEGER NOT NULL,
    buyer_user_id  INTEGER NOT NULL,
    invoice_id     TEXT NOT NULL UNIQUE,
    status         TEXT NOT NULL DEFAULT 'pending',
    tracking       TEXT,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL
);

-- Activation codes (per seller)
CREATE TABLE IF NOT EXISTS activation_codes (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    seller_id   INTEGER NOT NULL,
    code        TEXT NOT NULL UNIQUE,
    plan_id     INTEGER,
    value_days  INTEGER,
    max_uses    INTEGER NOT NULL DEFAULT 1,
    uses        INTEGER NOT NULL DEFAULT 0,
    expires_at  TEXT,
    created_at  TEXT NOT NULL
);

-- Code redemptions
CREATE TABLE IF NOT EXISTS code_redemptions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    code_id     INTEGER NOT NULL,
    user_id     INTEGER NOT NULL,
    redeemed_at TEXT NOT NULL
);

-- Users (for public mode + broadcasting, per seller optional)
CREATE TABLE IF NOT EXISTS users (
    user_id    INTEGER PRIMARY KEY,
    username   TEXT,
    first_seen TEXT NOT NULL,
    last_seen  TEXT NOT NULL
);
-- AI templates (per seller)
CREATE TABLE IF NOT EXISTS ai_templates (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    seller_id     INTEGER NOT NULL,
    name          TEXT NOT NULL,
    description   TEXT,
    system_prompt TEXT,
    user_prompt   TEXT NOT NULL,
    variables     TEXT,
    active        INTEGER NOT NULL DEFAULT 1,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    UNIQUE(seller_id, name)
);

-- Web dashboard accounts (per seller/admin/owner)
CREATE TABLE IF NOT EXISTS web_accounts (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    username      TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    salt          TEXT NOT NULL,
    role          TEXT NOT NULL,
    seller_id     INTEGER,
    created_at    TEXT NOT NULL
);

-- Web dashboard sessions
CREATE TABLE IF NOT EXISTS web_sessions (
    token_hash  TEXT PRIMARY KEY,
    account_id  INTEGER NOT NULL,
    role        TEXT NOT NULL,
    seller_id   INTEGER,
    expires_at  TEXT NOT NULL,
    created_at  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS ai_usage (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    seller_id         INTEGER NOT NULL,
    user_id           INTEGER,
    provider          TEXT NOT NULL,
    model             TEXT NOT NULL,
    template          TEXT,
    prompt_tokens     INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    total_tokens      INTEGER NOT NULL DEFAULT 0,
    estimated_cost    REAL NOT NULL DEFAULT 0,
    status            TEXT NOT NULL,
    error_message     TEXT,
    request_id        TEXT,
    created_at        TEXT NOT NULL
);

-- Refunds
CREATE TABLE IF NOT EXISTS refunds (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    seller_id   INTEGER NOT NULL,
    invoice_id  TEXT,
    user_id     INTEGER,
    amount      TEXT NOT NULL,
    asset       TEXT NOT NULL,
    reason      TEXT,
    status      TEXT NOT NULL DEFAULT 'pending',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

-- Payouts
CREATE TABLE IF NOT EXISTS payouts (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    seller_id    INTEGER NOT NULL,
    amount       TEXT NOT NULL,
    asset        TEXT NOT NULL,
    destination  TEXT,
    status       TEXT NOT NULL DEFAULT 'pending',
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);

-- Audit log
CREATE TABLE IF NOT EXISTS audit_log (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    actor_id     INTEGER NOT NULL,
    actor_role   TEXT NOT NULL,
    action       TEXT NOT NULL,
    target_type  TEXT NOT NULL,
    target_id    TEXT,
    metadata     TEXT,
    created_at   TEXT NOT NULL
);

-- API keys
CREATE TABLE IF NOT EXISTS api_keys (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id   INTEGER NOT NULL,
    name         TEXT,
    key_hash     TEXT NOT NULL UNIQUE,
    key_prefix   TEXT,
    scopes       TEXT,
    revoked      INTEGER NOT NULL DEFAULT 0,
    created_at   TEXT NOT NULL,
    expires_at   TEXT,
    last_used_at TEXT
);

-- Scheduled posts
CREATE TABLE IF NOT EXISTS scheduled_posts (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    seller_id        INTEGER NOT NULL,
    chat_id          TEXT NOT NULL,
    content          TEXT,
    kind             TEXT NOT NULL,
    scheduled_at     TEXT NOT NULL,
    status           TEXT NOT NULL DEFAULT 'pending',
    created_at       TEXT NOT NULL,
    posted_message_id INTEGER
);
CREATE INDEX IF NOT EXISTS idx_scheduled_posts_status_time ON scheduled_posts(status, scheduled_at);
-- Indexes
CREATE INDEX IF NOT EXISTS idx_invoices_status ON invoices(status);
CREATE INDEX IF NOT EXISTS idx_subscriptions_status_expires ON subscriptions(status, expires_at);
CREATE INDEX IF NOT EXISTS idx_products_seller ON products(seller_id);
CREATE INDEX IF NOT EXISTS idx_product_grants_seller ON product_grants(seller_id);
CREATE INDEX IF NOT EXISTS idx_orders_seller_status ON orders(seller_id, status);
CREATE INDEX IF NOT EXISTS idx_refunds_seller ON refunds(seller_id);
CREATE INDEX IF NOT EXISTS idx_payouts_seller ON payouts(seller_id);
CREATE INDEX IF NOT EXISTS idx_audit_actor ON audit_log(actor_id);
CREATE INDEX IF NOT EXISTS idx_audit_created ON audit_log(created_at);
CREATE INDEX IF NOT EXISTS idx_api_keys_account ON api_keys(account_id);
CREATE INDEX IF NOT EXISTS idx_api_keys_hash ON api_keys(key_hash);
