use std::net::SocketAddr;
use std::sync::Arc;
use teloxide::{
    dispatching::dialogue::{ErasedStorage, SqliteStorage, Storage, serializer::Json},
    payloads::CreateChatInviteLinkSetters,
    prelude::*,
};
use tracing_subscriber::EnvFilter;

use telegram_agent::{
    ai, api, config,
    config::Config,
    crypto,
    db::Db,
    dialogue::State,
    expire_subscriptions, handlers,
    handlers::registry::{CommandContext, CommandRegistry},
    mcp, metrics, roles, services, util, web,
};

type HandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// Bounded retries for background jobs (2026-10-25 review: "leaving for
/// retry" forever). After this many FAILED VERIFICATION/SEND attempts the
/// item is marked terminal-failed instead of being re-queued. A "not settled
/// yet" invoice release does NOT count — a buyer may legitimately pay days
/// later; only verification errors and misconfigurations age out.
const VERIFY_ATTEMPT_CAP: i64 = 10;
const POST_ATTEMPT_CAP: i64 = 10;

/// Record one failed verification attempt for an invoice and, at the cap,
/// move it to the terminal 'failed' state (pending_invoices() no longer
/// returns it), log loudly, and audit. Returns the new attempt count so the
/// caller can log bounded-retry context. A degraded counter (0) reads as
/// "never capped" — identical to the previous retry-forever behavior.
fn bump_invoice_failure(db: &Db, inv_id: &str, reason: &str) -> i64 {
    let attempts = db.bump_invoice_attempts(inv_id);
    if attempts >= VERIFY_ATTEMPT_CAP {
        db.update_invoice_status(inv_id, "failed");
        tracing::error!(
            "invoice {} failed terminally after {} verification attempts ({}); restore manually via the admin invoice-status API if needed",
            inv_id,
            attempts,
            reason
        );
        db.log_action(
            0,
            "system",
            "invoice_verify_failed",
            "invoice",
            inv_id,
            Some(&format!("attempts={} reason={}", attempts, reason)),
        );
    }
    attempts
}

#[tokio::main]
async fn main() {
    config::load_dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    // Pin the metrics uptime gauge to process start (L8 observability).
    metrics::note_process_start();

    let cfg = Config::from_env();
    if !cfg.is_configured() {
        eprintln!("Setup mode: open the web UI to configure (bot token / owner id missing).");
    }

    let db = match Db::new(&cfg.db_path) {
        Ok(d) => Arc::new(d),
        Err(e) => {
            eprintln!("DB error: {}", e);
            return;
        }
    };

    // Seed owner web account (idempotent) only when configured.
    if cfg.is_configured() {
        {
            let uname = cfg.web_username();
            // Never fall back to the (public) owner id as the password: when
            // WEB_PASSWORD is unset or empty, generate a random secret and
            // surface it once on stderr so the operator can log in.
            let pw = std::env::var("WEB_PASSWORD")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| {
                    let pw = util::gen_token(16);
                    eprintln!("Generated web dashboard password (WEB_PASSWORD was unset): {pw}");
                    pw
                });
            if db.get_web_account_by_username(&uname).is_none()
                && let Err(e) = db.create_web_account(&uname, &pw, "owner", None).await
            {
                tracing::warn!("failed to seed owner web account: {}", e);
            }
        }
    }

    // Ensure default channel from env is stored if not set.
    if db.get_config("channel").is_none()
        && let Some(ch) = &cfg.default_channel
    {
        db.set_config("channel", ch);
    }

    // v1.3.x claim flow: setup is skippable, so an instance may run with no
    // owner web account. Until one exists (and setup is complete), maintain
    // the one-time claim code and print it once, loudly — the same
    // CSPRNG-secret-printed-once pattern as the generated WEB_PASSWORD above
    // (stderr at startup; the operator copies it from the console). Whenever
    // an owner account exists the code is stale and cleared: claim mode can
    // never return. Web claim handlers (web.rs) compare what the browser
    // presents against `db.claim_code()` and waive the code for loopback
    // peers — see the ConnectInfo note on the serve call below.
    if let Some(code) = db.ensure_claim_code() {
        eprintln!(
            "First-run claim code: {code}\n  One-time code for claiming the owner account in the web console (required only from a non-loopback browser; localhost needs no code)."
        );
    }

    let crypto = cfg
        .crypto_token
        .clone()
        .map(crypto::CryptoPay::new)
        .map(Arc::new);

    let ai_service: Option<Arc<ai::AiService>> = if cfg.ai_enabled {
        let provider: Arc<dyn ai::AiProvider> = match cfg.ai_provider.as_str() {
            "anthropic" => {
                let key = cfg.ai_api_key.clone().unwrap_or_default();
                Arc::new(ai::anthropic::AnthropicProvider::new(key))
            }
            _ => {
                let key = cfg.ai_api_key.clone().unwrap_or_default();
                Arc::new(ai::openai::OpenAiProvider::new(
                    cfg.ai_base_url.clone(),
                    key,
                ))
            }
        };
        Some(Arc::new(ai::AiService {
            provider,
            model: cfg.ai_model.clone(),
            db: db.clone(),
            rate_limiter: ai::rate_limit::RateLimiter::new(20),
            daily_token_budget_global: cfg.ai_daily_token_budget,
            enabled: true,
        }))
    } else {
        None
    };

    let bot = if cfg.is_configured() {
        Bot::new(cfg.bot_token.clone())
    } else {
        Bot::new("0:setup-placeholder".to_string())
    };

    // Dialogue storage (persistent).
    let dialogue_path = std::path::Path::new(&cfg.db_path)
        .parent()
        .map(|p| p.join("dialogue.db"))
        .unwrap_or_else(|| std::path::PathBuf::from("./data/dialogue.db"));
    if let Some(parent) = dialogue_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let storage: Arc<ErasedStorage<State>> =
        SqliteStorage::open(dialogue_path.to_str().unwrap(), Json)
            .await
            .expect("open dialogue storage")
            .erase();

    let owner_id = cfg.owner_id;

    // Auto-selling background processor: verifies pending invoices and grants access.
    // Each invoice is claimed ('active' -> 'granting') before verification so
    // concurrent granters (this loop, /paid, the claim API) can never
    // double-grant; losers simply skip. Nothing is ever granted without a
    // verified 'paid' from the provider (fail-closed): unresolvable claims are
    // either released back to 'active' (not settled) or left 'granting' so the
    // next cycle retries (pending_invoices() includes 'granting').
    {
        let bot_bg = bot.clone();
        let db_bg = db.clone();
        let cfg_bg = cfg.clone();
        let crypto_bg = crypto.clone();
        tokio::spawn(async move {
            use std::time::Duration;
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let pending = db_bg.pending_invoices();
                for (inv_id, plan_name, uid, inv_seller) in pending {
                    // (a) Claim: only the winner verifies and grants.
                    if !db_bg.claim_invoice_for_grant(&inv_id) {
                        continue;
                    }
                    // (b) Resolve a crypto token; without one there is no way to
                    // verify payment — leave the claim for a later cycle, no grant.
                    let cp = if let Some(c) = &crypto_bg {
                        Some(c.clone())
                    } else {
                        db_bg
                            .get_config("crypto_token")
                            .or_else(|| cfg_bg.crypto_token.clone())
                            .map(crypto::CryptoPay::new)
                            .map(std::sync::Arc::new)
                    };
                    let cp = match cp {
                        Some(c) => c,
                        None => {
                            // Bounded retry (VERIFY_ATTEMPT_CAP): a missing
                            // token is a permanent misconfiguration, not a
                            // transient condition — age the invoice out.
                            let attempts =
                                bump_invoice_failure(&db_bg, &inv_id, "no crypto token configured");
                            if attempts < VERIFY_ATTEMPT_CAP {
                                tracing::warn!(
                                    "invoice {} claimed but no crypto token configured; leaving for retry (attempt {}/{})",
                                    inv_id,
                                    attempts,
                                    VERIFY_ATTEMPT_CAP
                                );
                            }
                            continue;
                        }
                    };
                    // (c) Verify with the provider; any error stays fail-closed.
                    let v = match cp.get_invoice(&inv_id).await {
                        Ok(v) => v,
                        Err(e) => {
                            // Bounded retry: N consecutive provider errors
                            // age the invoice out instead of looping forever.
                            let attempts = bump_invoice_failure(
                                &db_bg,
                                &inv_id,
                                &format!("provider verification error: {}", e),
                            );
                            if attempts < VERIFY_ATTEMPT_CAP {
                                tracing::warn!(
                                    "invoice {} verification failed: {}; leaving for retry (attempt {}/{})",
                                    inv_id,
                                    e,
                                    attempts,
                                    VERIFY_ATTEMPT_CAP
                                );
                            }
                            continue;
                        }
                    };
                    let paid = crate::api::extract_invoice_items(&v)
                        .iter()
                        .any(|x| x.get("status").and_then(|s| s.as_str()) == Some("paid"));
                    if !paid {
                        // Not settled yet: release the claim so future cycles
                        // (and /paid) re-verify cleanly.
                        db_bg.update_invoice_status(&inv_id, "active");
                        continue;
                    }
                    // (d-bis) Product invoices: grant the purchase and DM the
                    // delivery payload. Plan invoices take the subscription
                    // path below; the branch is selected by invoices.product_id.
                    if let Some((pid, kind, pname, delivery)) =
                        db_bg.invoice_product_delivery(&inv_id)
                    {
                        let target_user = uid.unwrap_or(0);
                        if target_user == 0 {
                            // Verified paid but nobody bound to deliver to:
                            // finalize so it stops re-queueing.
                            tracing::info!(
                                "invoice {} paid but has no bound user; finalizing without product grant",
                                inv_id
                            );
                            db_bg.finalize_invoice_paid(&inv_id);
                            continue;
                        }
                        if !db_bg.product_grant_exists(&inv_id) {
                            match db_bg.grant_product(inv_seller, target_user, pid, &inv_id) {
                                Ok(true) => {
                                    if let Err(e) = services::deliver::deliver(
                                        &bot_bg,
                                        target_user,
                                        &kind,
                                        &pname,
                                        delivery.as_deref(),
                                    )
                                    .await
                                    {
                                        tracing::warn!(
                                            "invoice {}: product '{}' granted but delivery DM failed: {}",
                                            inv_id,
                                            pname,
                                            e
                                        );
                                    }
                                }
                                // A concurrent granter (/paid, claim API) won
                                // the UNIQUE(invoice_id) race; nothing to do.
                                Ok(false) => {}
                                // e.g. out of stock: money settled, grant not —
                                // surface loudly and finalize so the invoice
                                // stops re-queueing (manual fulfillment).
                                Err(e) => {
                                    tracing::warn!(
                                        "invoice {}: product grant failed ({}); finalizing without delivery",
                                        inv_id,
                                        e
                                    );
                                }
                            }
                        }
                        // Terminal: verified paid + grant attempted.
                        db_bg.finalize_invoice_paid(&inv_id);
                        continue;
                    }
                    // (d) Idempotency: if a subscription already exists for this
                    // invoice, only finalize.
                    if db_bg.subscription_exists_for_invoice(&inv_id) {
                        db_bg.finalize_invoice_paid(&inv_id);
                        continue;
                    }
                    // (e) Grant: subscription + single-use invite link + notice.
                    let target_user = uid.unwrap_or(0);
                    if target_user == 0 {
                        // Verified paid but nobody is bound to grant to: finalize
                        // so it stops re-queueing instead of spinning forever.
                        tracing::info!(
                            "invoice {} paid but has no bound user; finalizing without grant",
                            inv_id
                        );
                        db_bg.finalize_invoice_paid(&inv_id);
                        continue;
                    }
                    // Seller-scoped plan lookup: the invoice's plan is
                    // resolved under the invoice's own seller, never a global
                    // same-name plan owned by someone else.
                    // Tuple: (id, price, asset, name, days, channel).
                    let plan_info = plan_name
                        .as_ref()
                        .and_then(|pn| db_bg.get_plan(inv_seller, pn));
                    let days = plan_info
                        .as_ref()
                        .map(|p| p.4)
                        .unwrap_or(30)
                        .clamp(1, crate::handlers::commands::MAX_PLAN_DAYS);
                    let seller_id = inv_seller;
                    let ch = plan_info
                        .as_ref()
                        .and_then(|p| p.5.clone())
                        .or_else(|| db_bg.seller_channel(seller_id))
                        .or_else(|| db_bg.get_config("channel"))
                        .or_else(|| cfg_bg.default_channel.clone())
                        .unwrap_or_default();
                    if ch.is_empty() {
                        // Paid but not grantable yet: leave the claim so the
                        // next cycle retries once a channel is configured.
                        // Bounded retry: an unresolvable channel is a
                        // misconfiguration, not a buyer-side condition.
                        let attempts = bump_invoice_failure(
                            &db_bg,
                            &inv_id,
                            &format!("no channel resolvable for seller {}", seller_id),
                        );
                        if attempts < VERIFY_ATTEMPT_CAP {
                            tracing::warn!(
                                "invoice {} paid but no channel resolvable for seller {}; leaving for retry (attempt {}/{})",
                                inv_id,
                                seller_id,
                                attempts,
                                VERIFY_ATTEMPT_CAP
                            );
                        }
                        continue;
                    }
                    let sub_id = db_bg.create_subscription(
                        seller_id,
                        target_user,
                        None,
                        &ch,
                        days,
                        Some(&inv_id),
                        None,
                    );
                    // grant invite link (checked expiry math; defensive even
                    // behind the clamp above)
                    let expire = chrono::Duration::try_days(days)
                        .and_then(|d| chrono::Utc::now().checked_add_signed(d))
                        .unwrap_or_else(chrono::Utc::now);
                    let link = bot_bg
                        .create_chat_invite_link(util::to_chat_id(&ch))
                        .member_limit(1)
                        .expire_date(expire)
                        .await
                        .ok()
                        .map(|l| l.invite_link);
                    if let Some(l) = link {
                        // Metrics hook only (L8): bot_messages_sent_total on
                        // the success arm.
                        match bot_bg
                            .send_message(
                                teloxide::types::ChatId(target_user),
                                format!("Payment confirmed. Access granted.\\nInvite: {}\\nSubscription #{}", l, sub_id),
                            )
                            .await
                        {
                            Err(e) => tracing::warn!("failed to send grant message: {}", e),
                            Ok(_) => metrics::bot_sent(),
                        }
                    }
                    // (f) Terminal: verified paid + granted.
                    db_bg.finalize_invoice_paid(&inv_id);
                }
            }
        });
    }
    // Scheduled posts background processor.
    {
        let bot_bg = bot.clone();
        let db_bg = db.clone();
        tokio::spawn(async move {
            use std::time::Duration;
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let due = db_bg.list_due_scheduled();
                for (
                    id,
                    seller_id,
                    chat_id,
                    content,
                    kind,
                    _scheduled_at,
                    _status,
                    _created_at,
                    _posted,
                ) in due
                {
                    let text = content.clone().unwrap_or_default();
                    match bot_bg
                        .send_message(util::to_chat_id(&chat_id), text.clone())
                        .await
                    {
                        Ok(sent) => {
                            let _ = db_bg.mark_scheduled_posted(id, sent.id.0);
                            let _ =
                                db_bg.log_post(seller_id, &chat_id, sent.id.0, &kind, Some(&text));
                            tracing::info!("scheduled post {} posted", id);
                            // Metrics hook only (L8): bot_messages_sent_total.
                            metrics::bot_sent();
                        }
                        Err(e) => {
                            // Bounded retry (POST_ATTEMPT_CAP): at the cap
                            // the post goes terminal-failed and leaves the
                            // due queue instead of retrying forever. Send
                            // failures are matched, never unwrapped — a
                            // broken chat must not panic the worker.
                            let attempts = db_bg.bump_scheduled_attempts(id);
                            if attempts >= POST_ATTEMPT_CAP {
                                db_bg.mark_scheduled_failed(id);
                                tracing::error!(
                                    "scheduled post {} failed terminally after {} send attempts: {}",
                                    id,
                                    attempts,
                                    e
                                );
                            } else {
                                tracing::warn!(
                                    "failed to post scheduled {} (attempt {}/{}): {}",
                                    id,
                                    attempts,
                                    POST_ATTEMPT_CAP,
                                    e
                                );
                            }
                        }
                    }
                }
            }
        });
    }
    // Expiry sweeper: periodically revoke expired subscriptions (kick + mark)
    // so lapsed members lose access even if nobody runs /expirecheck.
    {
        let bot_bg = bot.clone();
        let db_bg = db.clone();
        tokio::spawn(async move {
            use std::time::Duration;
            loop {
                tokio::time::sleep(Duration::from_secs(6 * 60 * 60)).await;
                let n = expire_subscriptions(&bot_bg, &db_bg).await;
                if n > 0 {
                    tracing::info!("expiry sweeper revoked {} subscriptions", n);
                }
            }
        });
    }
    let db_cmd = db.clone();
    let cfg_cmd = cfg.clone();
    let crypto_cmd = crypto.clone();
    let ai_cmd = ai_service.clone();
    let db_plugin = db.clone();
    let cfg_plugin = cfg.clone();
    let db_photo = db.clone();
    let cfg_photo = cfg.clone();
    let db_crypto = db.clone();
    let cfg_crypto = cfg.clone();

    // Plugin commands registered at startup (see handlers::registry). The
    // binary registers only the MCP approval commands (/approvals, /approve,
    // /deny); embedded hosts and forks can add theirs. Registration must
    // happen before the registry is frozen into an Arc, so the web state the
    // approval executors share with the MCP endpoint is built first.
    let base = cfg.web_base_path();
    let web_state = web::WebState {
        db: db.clone(),
        cfg: cfg.clone(),
        bot: bot.clone(),
        login_attempts: std::sync::Arc::new(
            std::sync::Mutex::new(std::collections::HashMap::new()),
        ),
        base_path: base.clone(),
    };
    let mut registry = CommandRegistry::new();
    mcp::register_commands(&mut registry, web_state.clone());
    let registry = Arc::new(registry);
    let registry_for_commands = registry.clone();

    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .enter_dialogue::<Message, ErasedStorage<State>, State>()
                .branch(
                    dptree::case![State::Idle]
                        .branch(dptree::entry().filter_command::<handlers::commands::Command>().endpoint(
                            move |bot: Bot,
                                  msg: Message,
                                  dialogue: Dialogue<State, ErasedStorage<State>>,
                                  cmd: handlers::commands::Command| {
                                let db = db_cmd.clone();
                                let cfg = cfg_cmd.clone();
                                let crypto = crypto_cmd.clone();
                                let ai = ai_cmd.clone();
                                let reg = registry_for_commands.clone();
                                async move {
                                    handlers::commands::cmd_handler(bot, msg, dialogue, db, cfg, crypto, ai, reg, cmd).await
                                }
                            },
                        ))
                        // Registered plugin commands: matched only when the
                        // text is `/name ...` naming a registered command
                        // (falls through otherwise), then role-gated like the
                        // built-ins before the handler runs.
                        .branch(
                            dptree::filter(
                                |reg: Arc<CommandRegistry>, msg: Message| {
                                    plugin_command_name(&msg, &reg).is_some()
                                },
                            )
                            .endpoint(
                                move |bot: Bot,
                                      msg: Message,
                                      reg: Arc<CommandRegistry>| {
                                    let db = db_plugin.clone();
                                    let cfg = cfg_plugin.clone();
                                    async move {
                                        plugin_command_handler(bot, msg, db, cfg, reg).await
                                    }
                                },
                            ),
                        )
                        .branch(dptree::endpoint(non_command_handler)),
                )
                .branch(dptree::case![State::AwaitingPhoto].endpoint(
                    move |bot: Bot, msg: Message, dialogue: Dialogue<State, ErasedStorage<State>>| {
                        let db = db_photo.clone();
                        let cfg = cfg_photo.clone();
                        async move { handlers::messages::awaiting_photo_handler(bot, msg, dialogue, db, cfg).await }
                    },
                ))
                // Catch-all for the remaining dialogue state (CryptoInvoice):
                // dptree's case! cannot bind struct-variant fields, and Idle/
                // AwaitingPhoto are handled above, so whatever falls through
                // here is the invoice wizard. The handler reads the state
                // itself from the dialogue.
                .branch(
                    dptree::endpoint(
                        move |bot: Bot,
                              msg: Message,
                              dialogue: Dialogue<State, ErasedStorage<State>>| {
                            let db = db_crypto.clone();
                            let cfg = cfg_crypto.clone();
                            async move {
                                handlers::messages::crypto_invoice_handler(bot, msg, dialogue, db, cfg)
                                    .await
                            }
                        },
                    ),
                ),
        )
        .branch(Update::filter_callback_query().endpoint(handlers::callbacks::callback_handler));

    // Inject dependencies.
    let mut dispatcher = Dispatcher::builder(bot.clone(), handler)
        .dependencies(dptree::deps![storage.clone(), owner_id, registry.clone()])
        .enable_ctrlc_handler()
        .build();

    // Web UI state.
    let web_router = web::router(web_state.clone());
    let api_router = api::router(web_state.clone());
    let web_router = web::mount(
        &base,
        web_router
            .merge(api_router)
            .merge(mcp::router(web_state))
            // 30s request ceiling for the whole control plane (L8). The bot
            // long-poll never passes through axum and the cryptopay webhook
            // is two DB writes, so nothing legitimate needs longer (see
            // web::REQUEST_TIMEOUT).
            .layer(axum::middleware::from_fn(web::request_timeout))
            // HTTP metrics outermost, so requests aborted by the timeout
            // layer are still counted (status_class="5xx"); labels come from
            // the matched route template only (see src/metrics.rs).
            .layer(axum::middleware::from_fn(metrics::track_http)),
    );
    let web_port: u16 = cfg.web_port();
    let web_bind = cfg.web_bind();
    let listener = match tokio::net::TcpListener::bind(format!("{web_bind}:{web_port}")).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("failed to bind {web_bind}:{web_port}: {e}");
            return;
        }
    };
    tracing::info!(
        "Web UI listening on http://{}:{} (localhost: http://127.0.0.1:{})",
        web_bind,
        web_port,
        web_port
    );
    if cfg.open_browser() {
        open_in_browser(&format!("http://127.0.0.1:{web_port}{base}"));
    }

    // Setup mode: without a bot token the poller cannot start (Telegram
    // rejects getMe and teloxide aborts the task), so run the web UI only —
    // configure there, then restart to bring the bot online.
    let bot_task = if cfg.is_configured() {
        Some(tokio::spawn(async move { dispatcher.dispatch().await }))
    } else {
        tracing::info!("bot polling disabled until configured (setup mode)");
        None
    };
    // ConnectInfo: the web claim handlers (v1.3.x) read the peer address via
    // ConnectInfo<SocketAddr> so codeless claiming works only from loopback —
    // without the make-service wrapper axum cannot see peer addresses.
    let web_task = tokio::spawn(async move {
        axum::serve(
            listener,
            web_router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });

    tokio::select! {
        _ = tokio::signal::ctrl_c() => { tracing::info!("shutdown signal received"); }
        _ = async {
            match bot_task {
                Some(t) => { let _ = t.await; }
                None => std::future::pending::<()>().await,
            }
        } => { tracing::info!("bot task ended"); }
        _ = web_task => { tracing::info!("web task ended"); }
    }
    // Flush the WAL into the main database file before exiting
    // (PRAGMA wal_checkpoint(TRUNCATE)) so backups and file copies see a
    // complete database instead of a main file plus a trailing -wal log.
    // `db` is still owned here — only clones moved into tasks/states — and
    // the background loops are parked in sleeps by the time a signal lands,
    // so the checkpoint normally runs uncontended. Best-effort: failures are
    // logged inside Db::checkpoint and never fatal.
    db.checkpoint();
    // Cancel remaining tasks.
    // (tasks are dropped on select exit; nothing further to do)
}

async fn non_command_handler(
    _bot: Bot,
    _msg: Message,
    _dialogue: Dialogue<State, teloxide::dispatching::dialogue::ErasedStorage<State>>,
) -> HandlerResult {
    Ok(())
}

/// The registered command named by `msg`, if the text starts with `/` and the
/// first word (minus the leading `/`) names a registered command.
fn plugin_command_name(msg: &Message, reg: &CommandRegistry) -> Option<String> {
    let text = msg.text()?;
    let first = text.split_whitespace().next()?;
    let name = first.strip_prefix('/')?;
    if name.is_empty() {
        return None;
    }
    reg.get(name).map(|_| name.to_string())
}

/// Dispatch a registered plugin command: parse the args (the text after the
/// command word), resolve the caller's role, and silently ignore callers below
/// the command's `min_role` — mirroring the built-in commands' gate.
async fn plugin_command_handler(
    bot: Bot,
    msg: Message,
    db: Arc<Db>,
    cfg: Config,
    reg: Arc<CommandRegistry>,
) -> HandlerResult {
    let Some(name) = plugin_command_name(&msg, &reg) else {
        return Ok(());
    };
    let Some(spec) = reg.get(&name) else {
        return Ok(());
    };
    let args = msg
        .text()
        .and_then(|t| t.split_once(char::is_whitespace))
        .map(|(_, rest)| rest.trim().to_string())
        .unwrap_or_default();
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);
    let role = roles::role_of(&db, user_id, cfg.owner_id);
    if role < spec.min_role {
        return Ok(());
    }
    let ctx = CommandContext {
        bot,
        msg,
        db,
        cfg,
        args,
        role,
    };
    (spec.handler)(ctx).await?;
    Ok(())
}

/// Open the dashboard in the system browser (desktop app UX). Fire-and-forget:
/// a failure to open a browser must never stop the server. Each platform
/// launcher is a fixed program invoked with the URL as a single argument —
/// no shell, no string-built command lines.
fn open_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("explorer").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();
    if let Err(e) = result {
        tracing::warn!("could not open browser: {e}");
    }
}
