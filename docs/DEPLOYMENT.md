# Deployment Guide

## Mount under an existing reverse proxy (subpath)

Set `WEB_BASE_PATH=/agent` in `.env`. The dashboard and API then live under
that path — every redirect, cookie, and form action is prefixed — so an
existing nginx/caddy serving other apps can add it without conflicts:

```nginx
# nginx — no path rewriting, pass through as-is
location /agent/ {
    proxy_pass http://127.0.0.1:8080;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
}
```

```caddy
# caddy
agent.example.com {
    reverse_proxy /agent/* 127.0.0.1:8080
}
```

Notes: `https://host/agent/api/v1/webhooks/cryptopay` is the webhook URL to
register with Crypto Pay; do not strip the prefix at the proxy; the root `/`
is intentionally not served when a base path is set. Also set
`WEB_BASE_PATH` **after** first-run setup or re-add it afterwards — the setup
form rewrites `.env` with a fixed key list (known issue, tracked).

## Embed into a Rust host

The crate is a library: an existing axum service can mount the whole app:

```rust
let app = telegram_agent::embed::AgentAppBuilder::new()
    .base_path("/agent")
    .db_path("data/host.db")
    .build()
    .await?;
let host = axum::Router::new()
    .route("/", axum::routing::get(|| async { "host" }))
    .merge(app.router());
```

Custom bot commands register without touching the core (`examples/plugin_command.rs`):

```rust
builder.command(CommandSpec {
    name: "echo".into(),
    description: "Echo text back".into(),
    min_role: roles::Role::User,
    handler: Arc::new(|ctx| Box::pin(async move { /* ... */ })),
});
```

## Systemd
```ini
[Unit]
Description=Telegram Business Platform
After=network.target

[Service]
Type=simple
User=telegram
WorkingDirectory=/opt/telegram-agent
EnvironmentFile=/opt/telegram-agent/.env
ExecStart=/opt/telegram-agent/telegram-agent
Restart=always

[Install]
WantedBy=multi-user.target
```

## Docker
```dockerfile
FROM rust:1.98 as builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
COPY --from=builder /app/target/release/telegram-agent /usr/local/bin/
CMD ["telegram-agent"]
```

## Backups
- Back up `data/agent.db` regularly.
- Store `.env` securely.