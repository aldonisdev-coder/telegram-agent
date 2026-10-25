#![allow(clippy::too_many_arguments)]
use crate::{
    config::Config, crypto::CryptoPay, db::Db, dialogue::State,
    handlers::registry::CommandRegistry, roles, util,
};
use chrono::{Duration, Utc};
use std::sync::Arc;
use teloxide::{
    payloads::SendMessageSetters,
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, MessageId},
    utils::command::BotCommands,
};

type HandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// Maximum subscription length (in days) any flow may create — 10 years.
/// Defined once here and referenced by the API layer and the binary
/// (`crate::handlers::commands::MAX_PLAN_DAYS`) so bot and web limits stay
/// identical. Public because the bin and fuzz targets are separate crates.
pub const MAX_PLAN_DAYS: i64 = 3650;

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
pub enum Command {
    /// Show help.
    #[command(aliases = ["h", "?"])]
    Help,
    /// Start / main menu.
    Start,
    /// Show configured target channel.
    Channel,
    /// List known channels.
    Channels,
    /// Set target channel: /setchannel @channel or id.
    SetChannel(String),
    /// Post text to channel: /post <text>.
    Post(String),
    /// Schedule a post: /schedule <@channel> <datetime> | <text>.
    Schedule(String),
    /// Post a photo (send photo next). /postphoto [caption].
    PostPhoto(String),
    /// Edit a posted message: /edit <msg_id> | <new text>.
    Edit(String),
    /// Delete a posted message: /delete [@channel] <msg_id>.
    Delete(String),
    /// Pin a message: /pin [@channel] <msg_id>.
    Pin(String),
    /// Unpin all messages in channel: /unpin [@channel].
    Unpin(String),
    /// Unpin specific message: /unpinmsg [@channel] <msg_id>.
    UnpinMsg(String),
    /// Create an invite link: /invite [@channel].
    Invite(String),
    /// Set channel description: /setdesc <text>.
    SetDesc(String),
    /// Set channel title: /settitle <text>.
    SetTitle(String),
    /// Channel info: /info [@channel].
    Info(String),
    /// Crypto Pay commands: /crypto <subcommand> ...
    Crypto(String),
    /// Inline menu.
    Menu,
    /// Bot self-setup: configure commands/profile. /setupbot
    SetupBot,
    /// Easy seller setup: /setup @channel
    Setup(String),
    /// Become a seller.
    BecomeSeller,
    /// List subscription plans. /plans
    Plans,
    /// Add plan: /planadd name | price | asset | days
    PlanAdd(String),
    /// Remove plan. /plandel name
    PlanDel(String),
    /// Add product: /productadd name | price | asset | kind | delivery | [stock]
    ProductAdd(String),
    /// List own products. /products
    Products,
    /// Deactivate a product. /productdel name
    ProductDel(String),
    /// List orders (sellers: own; admins: all). /orders
    Orders,
    /// Update an order: /orderset <order_id> <status> [tracking]
    OrderSet(String),
    /// Public shop listing. /shop
    Shop,
    /// Buy a plan or product: /buy name
    Buy(String),
    /// Claim payment: /paid invoice_id
    Paid(String),
    /// Redeem activation code: /redeem CODE
    Redeem(String),
    /// Generate codes: /gencodes plan | count | [days]
    GenCodes(String),
    /// List activation codes. /codes
    Codes,
    /// Grant access: /grant user_id | days
    Grant(String),
    /// Revoke access: /revoke user_id
    Revoke(String),
    /// List subscriptions. /subs
    Subs,
    /// Run expiry cleanup. /expirecheck
    ExpireCheck,
    /// Broadcast to all users: /broadcast <text>.
    Broadcast(String),
    /// AI generate: /ai <template> | var=val | ...
    Ai(String),
    /// Set web dashboard password: /setwebpassword <new>.
    SetWebPassword(String),
    /// Show web dashboard login info: /dashboard.
    Dashboard,
    /// Add platform admin (owner only): /addadmin <user_id> [username].
    AddAdmin(String),
    /// Remove platform admin (owner only): /removeadmin <user_id>.
    RemoveAdmin(String),
    /// List platform admins (owner only): /admins.
    Admins,
    /// List sellers (admin/owner): /sellers.
    Sellers,
    /// List web accounts (owner only): /webusers.
    WebUsers,
}

pub async fn cmd_handler(
    bot: Bot,
    msg: Message,
    dialogue: Dialogue<State, teloxide::dispatching::dialogue::ErasedStorage<State>>,
    db: Arc<Db>,
    cfg: Config,
    crypto: Option<Arc<CryptoPay>>,
    ai: Option<Arc<crate::ai::AiService>>,
    reg: Arc<CommandRegistry>,
    cmd: Command,
) -> HandlerResult {
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);
    // register user for public mode / broadcasting
    if user_id != 0 {
        let uname = msg.from.as_ref().and_then(|u| u.username.clone());
        db.upsert_user(user_id, uname.as_deref());
        // NOTE: sellers are NOT auto-created here. Seller status must come from
        // an explicit /becomeseller or /setup flow (or the admin API) so the
        // seller role — and the channels/privileges it unlocks — is always
        // opt-in.
    }
    let role = roles::role_of(&db, user_id, cfg.owner_id);
    let is_admin = role >= roles::Role::Admin;
    let is_seller = role >= roles::Role::Seller;
    let user_cmd = matches!(
        cmd,
        Command::Start
            | Command::Help
            | Command::Menu
            | Command::Plans
            | Command::Shop
            | Command::Buy(_)
            | Command::Paid(_)
            | Command::Redeem(_)
            | Command::Setup(_)
            | Command::BecomeSeller
    );
    let seller_cmd = matches!(
        cmd,
        Command::Channel
            | Command::Channels
            | Command::SetChannel(_)
            | Command::Post(_)
            | Command::Schedule(_)
            | Command::PostPhoto(_)
            | Command::Edit(_)
            | Command::Delete(_)
            | Command::Pin(_)
            | Command::Unpin(_)
            | Command::UnpinMsg(_)
            | Command::Invite(_)
            | Command::SetDesc(_)
            | Command::SetTitle(_)
            | Command::Info(_)
            | Command::Crypto(_)
            | Command::PlanAdd(_)
            | Command::PlanDel(_)
            | Command::ProductAdd(_)
            | Command::Products
            | Command::ProductDel(_)
            | Command::Orders
            | Command::OrderSet(_)
            | Command::Grant(_)
            | Command::Revoke(_)
            | Command::Subs
            | Command::ExpireCheck
            | Command::Broadcast(_)
            | Command::Ai(_)
            | Command::GenCodes(_)
            | Command::Codes
            | Command::SetWebPassword(_)
            | Command::Dashboard
    );
    let owner_cmd = matches!(
        cmd,
        Command::AddAdmin(_)
            | Command::RemoveAdmin(_)
            | Command::Admins
            | Command::Sellers
            | Command::WebUsers
    );
    let allowed = user_cmd
        || (is_seller && seller_cmd)
        || (is_admin && seller_cmd)
        || (role == roles::Role::Owner && owner_cmd);
    if !allowed {
        // silent ignore for unauthorized commands
        return Ok(());
    }

    let chat_id = msg.chat.id;
    match cmd {
        Command::Help => {
            let mut help = Command::descriptions().to_string();
            help.push_str(&reg.descriptions());
            bot.send_message(chat_id, help).await?;
        }
        Command::Start => {
            bot.send_message(chat_id, "Telegram Agent (owner-only). Use /menu or /help.")
                .await?;
            send_menu(&bot, chat_id).await?;
        }
        Command::Menu => {
            send_menu(&bot, chat_id).await?;
        }
        Command::Channel => {
            let ch = channel_of(&db, &cfg, Some(user_id), is_admin);
            bot.send_message(chat_id, format!("Default target channel: {}", ch))
                .await?;
        }
        Command::Channels => {
            let mut set = std::collections::BTreeSet::new();
            if let Some(c) = db.get_config("channel") {
                set.insert(c);
            }
            if let Some(c) = &cfg.default_channel {
                set.insert(c.clone());
            }
            for p in db.list_plans(None) {
                if let Some(c) = p.4 {
                    set.insert(c);
                }
            }
            if set.is_empty() {
                bot.send_message(chat_id, "No channels configured.").await?;
            } else {
                bot.send_message(
                    chat_id,
                    format!(
                        "Known channels:\n{}",
                        set.iter()
                            .map(|c| format!("- {}", c))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ),
                )
                .await?;
            }
        }
        Command::SetChannel(c) => {
            let c = c.trim().to_string();
            if c.is_empty() {
                bot.send_message(chat_id, "Usage: /setchannel @channel or id")
                    .await?;
            } else {
                db.set_seller_channel(user_id, &c);
                bot.send_message(chat_id, format!("Target channel set to: {}", c))
                    .await?;
            }
        }
        Command::Post(text) => {
            let (chan, rest) = split_channel_arg(&text);
            let text = rest.trim().to_string();
            if text.is_empty() {
                bot.send_message(chat_id, "Usage: /post [@channel] <text>")
                    .await?;
            } else {
                let ch = chan.unwrap_or_else(|| channel_of(&db, &cfg, Some(user_id), is_admin));
                match bot.send_message(util::to_chat_id(&ch), text.clone()).await {
                    Ok(sent) => {
                        let _ = db.log_post(user_id, &ch, sent.id.0, "text", Some(&text));
                        bot.send_message(
                            chat_id,
                            format!("Posted to {} (message_id={}).", ch, sent.id.0),
                        )
                        .await?;
                    }
                    Err(e) => {
                        bot.send_message(chat_id, format!("Failed: {}", e)).await?;
                    }
                }
            }
        }
        Command::Schedule(arg) => {
            let (chan, rest) = split_channel_arg(&arg);
            let ch = chan.unwrap_or_else(|| channel_of(&db, &cfg, Some(user_id), is_admin));
            let parts: Vec<&str> = rest.splitn(2, '|').collect();
            if parts.len() < 2 {
                bot.send_message(chat_id, "Usage: /schedule [@channel] <datetime> | <text>")
                    .await?;
            } else {
                let dt_str = parts[0].trim();
                let text = parts[1].trim().to_string();
                if text.is_empty() {
                    bot.send_message(chat_id, "Usage: /schedule [@channel] <datetime> | <text>")
                        .await?;
                } else {
                    let scheduled_at =
                        parse_schedule_datetime(dt_str).unwrap_or_else(|| dt_str.to_string());
                    let id = db.create_scheduled_post(user_id, &ch, &text, "text", &scheduled_at);
                    bot.send_message(
                        chat_id,
                        format!("Scheduled post #{} for {}.", id, scheduled_at),
                    )
                    .await?;
                }
            }
        }
        Command::PostPhoto(cap) => {
            let (chan, rest) = split_channel_arg(&cap);
            let cap = rest.trim().to_string();
            let ch = chan.unwrap_or_else(|| channel_of(&db, &cfg, Some(user_id), is_admin));
            db.set_config("pending_channel", &ch);
            dialogue.update(State::AwaitingPhoto).await?;
            if cap.is_empty() {
                db.set_config("pending_caption", "");
                bot.send_message(chat_id, format!("Send the photo now (target: {}).", ch))
                    .await?;
            } else {
                db.set_config("pending_caption", &cap);
                bot.send_message(chat_id, format!("Send the photo now (target: {}).", ch))
                    .await?;
            }
        }
        Command::Edit(arg) => {
            let (chan, rest) = split_channel_arg(&arg);
            let ch = chan.unwrap_or_else(|| channel_of(&db, &cfg, Some(user_id), is_admin));
            let parts: Vec<&str> = rest.splitn(2, '|').collect();
            if parts.len() < 2 {
                bot.send_message(chat_id, "Usage: /edit [@channel] <msg_id> | <new text>")
                    .await?;
            } else {
                let mid: i32 = match parts[0].trim().parse() {
                    Ok(v) => v,
                    Err(_) => {
                        bot.send_message(chat_id, "Invalid message id").await?;
                        return Ok(());
                    }
                };
                let newtext = parts[1].trim();
                match bot
                    .edit_message_text(util::to_chat_id(&ch), MessageId(mid), newtext)
                    .await
                {
                    Ok(_) => {
                        bot.send_message(chat_id, "Edited.").await?;
                    }
                    Err(e) => {
                        bot.send_message(chat_id, format!("Failed: {}", e)).await?;
                    }
                }
            }
        }
        Command::Delete(arg) => {
            // arg may be "@chan 123" or "123"
            let argstr = arg.clone();
            let (chan, rest) = split_channel_arg(&argstr);
            let ch = chan.unwrap_or_else(|| channel_of(&db, &cfg, Some(user_id), is_admin));
            let id: i64 = rest.trim().parse().unwrap_or(0);
            if id == 0 {
                bot.send_message(chat_id, "Usage: /delete [@channel] <msg_id>")
                    .await?;
                return Ok(());
            }
            match bot
                .delete_message(util::to_chat_id(&ch), MessageId(id as i32))
                .await
            {
                Ok(_) => {
                    bot.send_message(chat_id, "Deleted.").await?;
                }
                Err(e) => {
                    bot.send_message(chat_id, format!("Failed: {}", e)).await?;
                }
            }
        }
        Command::Pin(arg) => {
            let (chan, rest) = split_channel_arg(&arg);
            let ch = chan.unwrap_or_else(|| channel_of(&db, &cfg, Some(user_id), is_admin));
            let id: i64 = rest.trim().parse().unwrap_or(0);
            if id == 0 {
                bot.send_message(chat_id, "Usage: /pin [@channel] <msg_id>")
                    .await?;
                return Ok(());
            }
            match bot
                .pin_chat_message(util::to_chat_id(&ch), MessageId(id as i32))
                .await
            {
                Ok(_) => {
                    bot.send_message(chat_id, "Pinned.").await?;
                }
                Err(e) => {
                    bot.send_message(chat_id, format!("Failed: {}", e)).await?;
                }
            }
        }
        Command::Unpin(arg) => {
            let (chan, _rest) = split_channel_arg(&arg);
            let ch = chan.unwrap_or_else(|| channel_of(&db, &cfg, Some(user_id), is_admin));
            match bot.unpin_all_chat_messages(util::to_chat_id(&ch)).await {
                Ok(_) => {
                    bot.send_message(chat_id, "Unpinned all.").await?;
                }
                Err(e) => {
                    bot.send_message(chat_id, format!("Failed: {}", e)).await?;
                }
            }
        }
        Command::UnpinMsg(arg) => {
            let (chan, rest) = split_channel_arg(&arg);
            let ch = chan.unwrap_or_else(|| channel_of(&db, &cfg, Some(user_id), is_admin));
            let id: i64 = rest.trim().parse().unwrap_or(0);
            if id == 0 {
                bot.send_message(chat_id, "Usage: /unpinmsg [@channel] <msg_id>")
                    .await?;
                return Ok(());
            }
            match bot
                .unpin_chat_message(util::to_chat_id(&ch))
                .message_id(MessageId(id as i32))
                .await
            {
                Ok(_) => {
                    bot.send_message(chat_id, "Unpinned.").await?;
                }
                Err(e) => {
                    bot.send_message(chat_id, format!("Failed: {}", e)).await?;
                }
            }
        }
        Command::Invite(arg) => {
            let (chan, _) = split_channel_arg(&arg);
            let ch = chan.unwrap_or_else(|| channel_of(&db, &cfg, Some(user_id), is_admin));
            match bot.create_chat_invite_link(util::to_chat_id(&ch)).await {
                Ok(link) => {
                    bot.send_message(chat_id, format!("Invite: {}", link.invite_link))
                        .await?;
                }
                Err(e) => {
                    bot.send_message(chat_id, format!("Failed: {}", e)).await?;
                }
            }
        }
        Command::SetDesc(arg) => {
            let (chan, rest) = split_channel_arg(&arg);
            let ch = chan.unwrap_or_else(|| channel_of(&db, &cfg, Some(user_id), is_admin));
            let text = rest.trim().to_string();
            if text.is_empty() {
                bot.send_message(chat_id, "Usage: /setdesc [@channel] <text>")
                    .await?;
                return Ok(());
            }
            match bot
                .set_chat_description(util::to_chat_id(&ch))
                .description(text.clone())
                .await
            {
                Ok(_) => {
                    bot.send_message(chat_id, "Description updated.").await?;
                }
                Err(e) => {
                    bot.send_message(chat_id, format!("Failed: {}", e)).await?;
                }
            }
        }
        Command::SetTitle(arg) => {
            let (chan, rest) = split_channel_arg(&arg);
            let ch = chan.unwrap_or_else(|| channel_of(&db, &cfg, Some(user_id), is_admin));
            let text = rest.trim().to_string();
            if text.is_empty() {
                bot.send_message(chat_id, "Usage: /settitle [@channel] <text>")
                    .await?;
                return Ok(());
            }
            match bot
                .set_chat_title(util::to_chat_id(&ch), text.clone())
                .await
            {
                Ok(_) => {
                    bot.send_message(chat_id, "Title updated.").await?;
                }
                Err(e) => {
                    bot.send_message(chat_id, format!("Failed: {}", e)).await?;
                }
            }
        }
        Command::Info(arg) => {
            let (chan, _) = split_channel_arg(&arg);
            let ch = chan.unwrap_or_else(|| channel_of(&db, &cfg, Some(user_id), is_admin));
            match bot.get_chat(util::to_chat_id(&ch)).await {
                Ok(c) => {
                    let title = c.title().unwrap_or("(no title)");
                    let desc = c.description().unwrap_or("");
                    let members = bot.get_chat_member_count(util::to_chat_id(&ch)).await.ok();
                    bot.send_message(
                        chat_id,
                        format!(
                            "Title: {}\nDescription: {}\nMembers: {:?}",
                            title, desc, members
                        ),
                    )
                    .await?;
                }
                Err(e) => {
                    bot.send_message(chat_id, format!("Failed: {}", e)).await?;
                }
            }
        }
        Command::Crypto(arg) => {
            handle_crypto(&bot, chat_id, &db, &cfg, crypto.clone(), user_id, &arg).await?;
        }
        Command::SetupBot => {
            setup_bot_profile(&bot).await?;
            bot.send_message(chat_id, "Bot profile/commands configured.")
                .await?;
        }
        Command::Plans => {
            // Seller-scoped: sellers see their own catalogue (incl. inactive),
            // admins keep the global view, plain users see the active-only
            // shop lines across sellers.
            let plans = if role == roles::Role::Seller {
                db.list_plans(Some(user_id))
            } else if is_admin {
                db.list_plans(None)
            } else {
                db.list_plans(None).into_iter().filter(|p| p.5).collect()
            };
            if plans.is_empty() {
                bot.send_message(chat_id, "No plans configured.").await?;
            } else {
                let mut out = String::from("Plans:\n");
                for (name, price, asset, days, _ch, active, _seller) in plans {
                    out.push_str(&format!(
                        "- {} : {} {} / {} days {}\n",
                        name,
                        price,
                        asset,
                        days,
                        if active { "" } else { "(inactive)" }
                    ));
                }
                bot.send_message(chat_id, out).await?;
            }
        }
        Command::PlanAdd(arg) => match parse_plan_add(&arg) {
            Err(PlanAddError::Usage) => {
                bot.send_message(chat_id, "Usage: /planadd name | price | asset | days")
                    .await?;
            }
            Err(PlanAddError::Days(raw)) => {
                bot.send_message(
                    chat_id,
                    format!("Invalid days '{}'. Must be 1..={}.", raw, MAX_PLAN_DAYS),
                )
                .await?;
            }
            Ok((name, price, asset, days)) => {
                let ch = channel_of(&db, &cfg, Some(user_id), is_admin);
                db.add_plan(user_id, name, price, asset, days, Some(&ch));
                bot.send_message(chat_id, format!("Plan '{}' added.", name))
                    .await?;
            }
        },
        Command::PlanDel(name) => {
            db.remove_plan(user_id, name.trim());
            bot.send_message(chat_id, "Plan removed.").await?;
        }
        Command::ProductAdd(arg) => {
            // /productadd name | price | asset | kind | delivery | [stock]
            match parse_product_add(&arg) {
                Err(ProductAddError::Usage) => {
                    bot.send_message(
                        chat_id,
                        "Usage: /productadd name | price | asset | kind | delivery | [stock]\nkinds: digital_link, digital_file, physical; delivery '-' if none; stock '-' or omitted = unlimited",
                    )
                    .await?;
                }
                Err(ProductAddError::Kind(kind)) => {
                    bot.send_message(
                        chat_id,
                        format!(
                            "Invalid kind '{}'. Must be digital_link, digital_file or physical.",
                            kind
                        ),
                    )
                    .await?;
                }
                Err(ProductAddError::Delivery) => {
                    bot.send_message(
                        chat_id,
                        "Digital products require a delivery payload (link/text/file_id).",
                    )
                    .await?;
                }
                Err(ProductAddError::Stock(s)) => {
                    bot.send_message(
                        chat_id,
                        format!(
                            "Invalid stock '{}'. Use an integer, or '-' for unlimited.",
                            s
                        ),
                    )
                    .await?;
                }
                Ok((name, price, asset, kind, delivery, stock)) => {
                    match db.add_product(user_id, name, price, asset, kind, delivery, stock) {
                        Ok(id) => {
                            bot.send_message(
                                chat_id,
                                format!(
                                    "Product '{}' added (#{}). Kind: {}. Stock: {}.",
                                    name,
                                    id,
                                    kind,
                                    stock
                                        .map(|s| s.to_string())
                                        .unwrap_or_else(|| "unlimited".to_string())
                                ),
                            )
                            .await?;
                        }
                        Err(e) => {
                            // e.g. UNIQUE(seller_id, name) collision.
                            bot.send_message(chat_id, format!("Error: {}", e)).await?;
                        }
                    }
                }
            }
        }
        Command::Products => {
            let products = db.list_products(if is_admin { None } else { Some(user_id) });
            if products.is_empty() {
                bot.send_message(chat_id, "No products. Use /productadd.")
                    .await?;
            } else {
                let mut out = String::from("Products (name | price | kind | stock | state):\n");
                for (_id, _seller, name, price, asset, kind, _delivery, stock, active) in products {
                    out.push_str(&format!(
                        "- {} : {} {} | {} | {} | {}\n",
                        name,
                        price,
                        asset,
                        kind,
                        stock
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| "unlimited".to_string()),
                        if active != 0 { "active" } else { "inactive" }
                    ));
                }
                bot.send_message(chat_id, out).await?;
            }
        }
        Command::ProductDel(name) => {
            let name = name.trim();
            if db.get_product(user_id, name).is_none() {
                bot.send_message(chat_id, "Product not found.").await?;
            } else {
                db.set_product_active(user_id, name, false);
                bot.send_message(chat_id, "Product deactivated.").await?;
            }
        }
        Command::Orders => {
            // Seller-side fulfillment view: sellers see their own orders,
            // Admin/Owner see all. Buyers get their receipt by DM.
            let orders = db.list_orders(if is_admin { None } else { Some(user_id) });
            if orders.is_empty() {
                bot.send_message(chat_id, "No orders.").await?;
            } else {
                let mut out = String::from("Orders (order_id — status — buyer — tracking):\n");
                for (order_id, _seller, buyer, status, tracking, _created) in orders {
                    out.push_str(&format!(
                        "{} — {} — {} — {}\n",
                        order_id,
                        status,
                        buyer,
                        tracking.unwrap_or_else(|| "-".to_string())
                    ));
                }
                bot.send_message(chat_id, out).await?;
            }
        }
        Command::OrderSet(arg) => {
            // /orderset <order_id> <status> [tracking...]: status always set;
            // tracking joined from the remaining words when present. Scopes to
            // the caller's own orders (set_order_status/set_order_tracking).
            match parse_order_set(&arg) {
                None => {
                    bot.send_message(chat_id, "Usage: /orderset <order_id> <status> [tracking]")
                        .await?;
                }
                Some((order_id, status, tracking)) => {
                    if !db.set_order_status(user_id, order_id, status) {
                        bot.send_message(
                            chat_id,
                            "Order not found or not yours. Status must be one of pending|shipped|delivered|refunded|cancelled.",
                        )
                        .await?;
                    } else if let Some(tracking) = tracking {
                        db.set_order_tracking(user_id, order_id, &tracking);
                        bot.send_message(
                            chat_id,
                            format!(
                                "Order {} updated: {} — tracking {}",
                                order_id, status, tracking
                            ),
                        )
                        .await?;
                    } else {
                        bot.send_message(
                            chat_id,
                            format!("Order {} updated: {}.", order_id, status),
                        )
                        .await?;
                    }
                }
            }
        }
        Command::Shop => {
            // Public catalogue: active products across all sellers, one line
            // each. Delivery payloads are never shown here — they ARE the
            // paid content.
            let products = db.list_products(None).into_iter().filter(|p| p.8 != 0);
            let mut out = String::from("Shop:\n");
            let mut n = 0;
            for (_id, _seller, name, price, asset, kind, _delivery, _stock, _active) in products {
                out.push_str(&format!(
                    "- {} — {} {} ({})\n",
                    name,
                    price,
                    asset,
                    match kind.as_str() {
                        "physical" => "product",
                        _ => "digital",
                    }
                ));
                n += 1;
            }
            if n == 0 {
                bot.send_message(chat_id, "Shop is empty.").await?;
            } else {
                bot.send_message(chat_id, out).await?;
            }
        }
        Command::Buy(name) => {
            // Multiplexed /buy: resolves the name across products AND plans.
            // Exactly one match buys it; zero matches or an ambiguous name
            // (>1 item, no seller disambiguation syntax in v1.2) errors out.
            let name = name.trim();
            if name.is_empty() {
                bot.send_message(chat_id, "Usage: /buy <name> (see /shop and /plans)")
                    .await?;
                return Ok(());
            }
            // Seller-scoped /buy resolution over the (seller_id, name) plan
            // namespace (2026-10-25 review HIGH). See resolve_buy_candidates
            // for the B5 fallback order.
            let (plans, mut products) = resolve_buy_candidates(&db, role, user_id, name);
            match (
                products.len() + plans.len(),
                products.pop(),
                plans.into_iter().next(),
            ) {
                (0, _, _) => {
                    bot.send_message(chat_id, "Not found. Use /shop or /plans.")
                        .await?;
                }
                (1, Some(product), _) => {
                    let (pid, p_seller, pname, price, asset, _kind, _delivery, stock, _active) =
                        product;
                    if stock == Some(0) {
                        bot.send_message(chat_id, format!("Product '{}' is out of stock.", pname))
                            .await?;
                        return Ok(());
                    }
                    let cp = crypto_or_load(&db, &cfg, crypto.clone());
                    match cp {
                        Some(c) => {
                            let desc = format!("Product: {}", pname);
                            match c
                                .create_invoice(&price, &asset, Some(&desc), Some("viewItem"), None)
                                .await
                            {
                                Ok(v) => {
                                    let inv_id = v
                                        .get("invoice_id")
                                        .map(|x| x.to_string())
                                        .unwrap_or_default();
                                    let pay_url =
                                        v.get("pay_url").and_then(|x| x.as_str()).unwrap_or("");
                                    if !inv_id.is_empty() {
                                        db.save_invoice(
                                            &inv_id,
                                            p_seller,
                                            &price,
                                            &asset,
                                            Some(&desc),
                                            "active",
                                            Some(pay_url),
                                            None,
                                            Some(user_id),
                                            Some(pid),
                                        );
                                    }
                                    bot.send_message(chat_id, format!("Invoice created for product '{}'.\nPay: {}\nThen run /paid {}", pname, pay_url, inv_id)).await?;
                                }
                                Err(e) => {
                                    bot.send_message(chat_id, format!("Error: {}", e)).await?;
                                }
                            }
                        }
                        None => {
                            bot.send_message(chat_id, "No crypto token configured.")
                                .await?;
                        }
                    }
                }
                (1, None, Some(plan)) => {
                    // Plan tuple: (name, price, asset, days, channel, active, seller).
                    let (_pname, price, asset, _days, _ch, _active, plan_seller) = plan;
                    let cp = crypto_or_load(&db, &cfg, crypto.clone());
                    match cp {
                        Some(c) => {
                            let desc = format!("Plan: {}", name);
                            match c
                                .create_invoice(&price, &asset, Some(&desc), Some("viewItem"), None)
                                .await
                            {
                                Ok(v) => {
                                    let inv_id = v
                                        .get("invoice_id")
                                        .map(|x| x.to_string())
                                        .unwrap_or_default();
                                    let pay_url =
                                        v.get("pay_url").and_then(|x| x.as_str()).unwrap_or("");
                                    if !inv_id.is_empty() {
                                        db.save_invoice(
                                            &inv_id,
                                            plan_seller,
                                            &price,
                                            &asset,
                                            Some(&desc),
                                            "active",
                                            Some(pay_url),
                                            Some(name),
                                            Some(user_id),
                                            None,
                                        );
                                    }
                                    bot.send_message(chat_id, format!("Invoice created for plan '{}'.\nPay: {}\nThen run /paid {}", name, pay_url, inv_id)).await?;
                                }
                                Err(e) => {
                                    bot.send_message(chat_id, format!("Error: {}", e)).await?;
                                }
                            }
                        }
                        None => {
                            bot.send_message(chat_id, "No crypto token configured.")
                                .await?;
                        }
                    }
                }
                _ => {
                    bot.send_message(
                        chat_id,
                        format!("'{}' is ambiguous (multiple items or sellers). Please contact the seller.", name),
                    )
                    .await?;
                }
            }
        }
        Command::Paid(inv) => {
            let inv = inv.trim();
            let info = db.get_invoice(inv);
            match info {
                Some((_amount, _asset, status, _desc, plan_name, uid, inv_seller)) => {
                    if status == "paid" {
                        bot.send_message(chat_id, "Already processed.").await?;
                        return Ok(());
                    }
                    // Invoice user binding: an invoice bound to a user may only
                    // be completed by that user; an unbound invoice only by
                    // Admin/Owner (never a random caller granting themselves).
                    let binding_ok = match uid {
                        Some(u) => u == user_id || is_admin,
                        None => is_admin,
                    };
                    if !binding_ok {
                        bot.send_message(chat_id, "You may not complete this invoice.")
                            .await?;
                        return Ok(());
                    }
                    // Claim (active -> granting) so the background granter and
                    // the claim API cannot double-grant while we verify.
                    if !db.claim_invoice_for_grant(inv) {
                        bot.send_message(
                            chat_id,
                            "Invoice is already being processed. Try again shortly.",
                        )
                        .await?;
                        return Ok(());
                    }
                    // Fail-closed: no crypto token means no payment
                    // verification, so no grant. Release the claim for a later
                    // attempt.
                    let cp = crypto_or_load(&db, &cfg, crypto.clone());
                    let c = match cp {
                        Some(c) => c,
                        None => {
                            db.update_invoice_status(inv, "active");
                            bot.send_message(
                                chat_id,
                                "Payment verification unavailable: no crypto token configured.",
                            )
                            .await?;
                            return Ok(());
                        }
                    };
                    match c.get_invoice(inv).await {
                        Err(e) => {
                            // Verification error: release the claim; the
                            // background granter retries. Never grant on doubt.
                            db.update_invoice_status(inv, "active");
                            bot.send_message(
                                chat_id,
                                format!("Could not verify payment ({}). Try again later.", e),
                            )
                            .await?;
                        }
                        Ok(v) => {
                            let paid = crate::api::extract_invoice_items(&v)
                                .iter()
                                .any(|x| x.get("status").and_then(|s| s.as_str()) == Some("paid"));
                            if !paid {
                                db.update_invoice_status(inv, "active");
                                bot.send_message(chat_id, "Invoice not paid yet.").await?;
                                return Ok(());
                            }
                            // Verified paid. Product invoices take the
                            // delivery path; plan invoices the subscription
                            // path. The branch is selected by
                            // invoices.product_id.
                            if let Some((pid, kind, pname, delivery)) =
                                db.invoice_product_delivery(inv)
                            {
                                let target_user = uid.unwrap_or(user_id);
                                if db.product_grant_exists(inv) {
                                    db.finalize_invoice_paid(inv);
                                    bot.send_message(
                                        chat_id,
                                        "Payment confirmed (product already delivered).",
                                    )
                                    .await?;
                                    return Ok(());
                                }
                                match db.grant_product(inv_seller, target_user, pid, inv) {
                                    Ok(true) => {
                                        // Physical goods also open a
                                        // fulfillment order the seller
                                        // manages with /orders. Best-effort:
                                        // the grant is already durable, so a
                                        // failure (e.g. duplicate invoice) is
                                        // only logged.
                                        if kind == "physical"
                                            && let Err(e) =
                                                db.create_order(inv_seller, pid, target_user, inv)
                                        {
                                            tracing::warn!(
                                                "invoice {}: order creation failed ({})",
                                                inv,
                                                e
                                            );
                                        }
                                        match crate::services::deliver::deliver(
                                            &bot,
                                            target_user,
                                            &kind,
                                            &pname,
                                            delivery.as_deref(),
                                        )
                                        .await
                                        {
                                            Ok(()) => {
                                                bot.send_message(
                                                    chat_id,
                                                    format!(
                                                        "Payment confirmed. Product '{}' delivered to your DM.",
                                                        pname
                                                    ),
                                                )
                                                .await?;
                                            }
                                            Err(e) => {
                                                bot.send_message(
                                                    chat_id,
                                                    format!(
                                                        "Payment confirmed, but delivery failed: {}. Contact the seller.",
                                                        e
                                                    ),
                                                )
                                                .await?;
                                            }
                                        }
                                    }
                                    // A concurrent granter won the
                                    // UNIQUE(invoice_id) race; nothing to do.
                                    Ok(false) => {}
                                    // e.g. out of stock: the money is settled
                                    // but the grant is not — surface it and
                                    // finalize so the invoice stops
                                    // re-queueing.
                                    Err(e) => {
                                        tracing::warn!(
                                            "invoice {}: product grant failed ({})",
                                            inv,
                                            e
                                        );
                                        bot.send_message(
                                            chat_id,
                                            format!(
                                                "Payment confirmed, but grant failed: {}. Contact the seller.",
                                                e
                                            ),
                                        )
                                        .await?;
                                    }
                                }
                                db.finalize_invoice_paid(inv);
                                return Ok(());
                            }
                            // Verified paid. Grant once (idempotent), then
                            // finalize.
                            if db.subscription_exists_for_invoice(inv) {
                                db.finalize_invoice_paid(inv);
                                bot.send_message(
                                    chat_id,
                                    "Payment confirmed (subscription already granted).",
                                )
                                .await?;
                                return Ok(());
                            }
                            let target_user = uid.unwrap_or(user_id);
                            // The channel must belong to the invoice's seller
                            // (via its plan) — never a global default.
                            let (days, ch) = if let Some(pn) = &plan_name
                                && let Some((_pid, _pr, _as, _nm, d, c)) =
                                    db.get_plan(inv_seller, pn)
                            {
                                (
                                    d,
                                    c.unwrap_or_else(|| {
                                        db.seller_channel(inv_seller).unwrap_or_default()
                                    }),
                                )
                            } else {
                                (30i64, db.seller_channel(inv_seller).unwrap_or_default())
                            };
                            let days = days.clamp(1, MAX_PLAN_DAYS);
                            if ch.is_empty() {
                                // Paid but ungrantable here: leave 'granting' so
                                // the background granter retries once the
                                // seller configures a channel.
                                bot.send_message(
                                    chat_id,
                                    "Payment confirmed, but no channel is configured to grant access. It will be granted automatically once configured.",
                                )
                                .await?;
                                return Ok(());
                            }
                            let sub_id = db.create_subscription(
                                inv_seller,
                                target_user,
                                None,
                                &ch,
                                days,
                                Some(inv),
                                None,
                            );
                            let link = grant_access(&bot, &ch, target_user, days).await;
                            match link {
                                Ok(l) => bot.send_message(chat_id, format!("Payment confirmed. Access granted.\nInvite: {}\nSubscription #{}", l, sub_id)).await?,
                                Err(e) => bot.send_message(chat_id, format!("Granted but invite failed: {}", e)).await?,
                            };
                            db.finalize_invoice_paid(inv);
                        }
                    }
                }
                None => {
                    bot.send_message(chat_id, "Unknown invoice.").await?;
                }
            }
        }
        Command::Redeem(code) => {
            let code = code.trim();
            match db.redeem_code(code, user_id) {
                Ok((code_seller, plan_id, value_days)) => {
                    // Days and channel come from the code's own plan/seller —
                    // never from a global default the seller does not control
                    // (plain users redeeming must still reach the seller's
                    // channel).
                    let (seller_id, d, ch) = if let Some(pid) = plan_id {
                        if let Some((sid, _price, _asset, _name, days, ch)) = db.get_plan_by_id(pid)
                        {
                            (
                                sid,
                                days,
                                ch.unwrap_or_else(|| db.seller_channel(sid).unwrap_or_default()),
                            )
                        } else {
                            (
                                code_seller,
                                value_days.unwrap_or(30),
                                db.seller_channel(code_seller).unwrap_or_default(),
                            )
                        }
                    } else {
                        (
                            code_seller,
                            value_days.unwrap_or(30),
                            db.seller_channel(code_seller).unwrap_or_default(),
                        )
                    };
                    let d = d.clamp(1, MAX_PLAN_DAYS);
                    if ch.is_empty() {
                        bot.send_message(
                            chat_id,
                            "Code redeemed, but the seller has no channel configured. Contact the seller.",
                        )
                        .await?;
                        return Ok(());
                    }
                    let sub_id =
                        db.create_subscription(seller_id, user_id, plan_id, &ch, d, None, None);
                    let link = grant_access(&bot, &ch, user_id, d).await;
                    match link {
                        Ok(l) => {
                            bot.send_message(
                                chat_id,
                                format!(
                                    "Code redeemed. Access granted.\nInvite: {}\nSubscription #{}",
                                    l, sub_id
                                ),
                            )
                            .await?
                        }
                        Err(e) => {
                            bot.send_message(chat_id, format!("Redeemed but invite failed: {}", e))
                                .await?
                        }
                    };
                }
                Err(e) => {
                    bot.send_message(chat_id, format!("Redeem failed: {}", e))
                        .await?;
                }
            }
        }
        Command::GenCodes(arg) => {
            match parse_gen_codes(&arg) {
                None => {
                    bot.send_message(chat_id, "Usage: /gencodes plan | count | [days]")
                        .await?;
                }
                Some((plan_name, count, days)) => {
                    // Seller-scoped lookup: the caller may only mint codes against
                    // their own plans.
                    let (seller_id, plan_id, val_days) = match db.get_plan(user_id, plan_name) {
                        Some((pid, _p, _a, _n, d, _c)) => (user_id, Some(pid), Some(d)),
                        None => (user_id, None, days),
                    };
                    let mut out = String::from("Generated codes:\n");
                    for _ in 0..count.min(50) {
                        let code = db.create_code(seller_id, plan_id, val_days, 1, None);
                        out.push_str(&code);
                        out.push('\n');
                    }
                    bot.send_message(chat_id, out).await?;
                }
            }
        }
        Command::Codes => {
            let codes = db.list_codes(if is_admin { None } else { Some(user_id) }, 50);
            if codes.is_empty() {
                bot.send_message(chat_id, "No codes.").await?;
            } else {
                let mut out = String::from("Codes (code | plan_id | value_days | uses/max):\n");
                for (code, pid, vd, uses, max, _seller) in codes {
                    out.push_str(&format!(
                        "{} | {:?} | {:?} | {}/{}\n",
                        code, pid, vd, uses, max
                    ));
                }
                bot.send_message(chat_id, out).await?;
            }
        }
        Command::Grant(arg) => {
            let parts: Vec<&str> = arg.split('|').map(|s| s.trim()).collect();
            if parts.len() < 2 {
                bot.send_message(chat_id, "Usage: /grant user_id | days")
                    .await?;
            } else {
                let uid: i64 = parts[0].parse().unwrap_or(0);
                let days: i64 = parts[1].parse().unwrap_or(30).clamp(1, MAX_PLAN_DAYS);
                if uid == 0 {
                    bot.send_message(chat_id, "Invalid user id.").await?;
                } else {
                    // Sellers grant only into their own channel; Admin/Owner
                    // may fall back to the global default.
                    let ch = channel_of(&db, &cfg, Some(user_id), is_admin);
                    if ch.is_empty() {
                        bot.send_message(
                            chat_id,
                            "No channel configured. Use /setup @channel first.",
                        )
                        .await?;
                        return Ok(());
                    }
                    let sub_id = db.create_subscription(user_id, uid, None, &ch, days, None, None);
                    let link = grant_access(&bot, &ch, uid, days).await;
                    match link {
                        Ok(l) => {
                            bot.send_message(
                                chat_id,
                                format!("Granted. Invite: {}\nSub #{}", l, sub_id),
                            )
                            .await?
                        }
                        Err(e) => {
                            bot.send_message(chat_id, format!("Granted but invite failed: {}", e))
                                .await?
                        }
                    };
                }
            }
        }
        Command::Revoke(arg) => {
            let uid: i64 = arg.trim().parse().unwrap_or(0);
            if uid == 0 {
                bot.send_message(chat_id, "Usage: /revoke user_id").await?;
            } else {
                let ch = channel_of(&db, &cfg, Some(user_id), is_admin);
                // ban then unban (kick)
                let cid = util::to_chat_id(&ch);
                let _ = bot
                    .ban_chat_member(cid.clone(), teloxide::types::UserId(uid as u64))
                    .await;
                let _ = bot
                    .unban_chat_member(cid, teloxide::types::UserId(uid as u64))
                    .await;
                bot.send_message(chat_id, format!("Revoked access for {}.", uid))
                    .await?;
            }
        }
        Command::Subs => {
            let subs = db.list_subscriptions(if is_admin { None } else { Some(user_id) }, 50);
            if subs.is_empty() {
                bot.send_message(chat_id, "No subscriptions.").await?;
            } else {
                let mut out = String::from("Subs (id | user | channel | status | expires):\n");
                for (id, _seller, uid, ch, st, exp) in subs {
                    out.push_str(&format!("{} | {} | {} | {} | {}\n", id, uid, ch, st, exp));
                }
                bot.send_message(chat_id, out).await?;
            }
        }
        Command::ExpireCheck => {
            // Shared with the periodic expiry sweeper in main.rs so the
            // kick+mark behavior stays single-sourced.
            let n = crate::expire_subscriptions(&bot, &db).await;
            bot.send_message(chat_id, format!("Expired and revoked: {}", n))
                .await?;
        }
        Command::Broadcast(text) => {
            if !is_admin {
                return Ok(());
            }
            let text = text.trim().to_string();
            if text.is_empty() {
                bot.send_message(chat_id, "Usage: /broadcast <text>")
                    .await?;
            } else {
                let users = if role == roles::Role::Seller {
                    db.list_users_for_seller(user_id)
                } else {
                    db.list_users()
                };
                // Send loop shared with the MCP broadcast tool (crate::broadcast_to_users).
                let sent = crate::broadcast_to_users(&bot, &users, &text).await;
                bot.send_message(chat_id, format!("Broadcast sent to {} users.", sent))
                    .await?;
            }
        }
        Command::Ai(arg) => {
            if !is_admin {
                return Ok(());
            }
            if ai.is_none() {
                bot.send_message(chat_id, "AI is disabled. Set AI_ENABLED=1 and AI_API_KEY.")
                    .await?;
                return Ok(());
            }
            let ai = ai.as_ref().unwrap();
            // parse: template | key=val | ...
            let parts: Vec<&str> = arg.split('|').map(|s| s.trim()).collect();
            if parts.is_empty() || parts[0].is_empty() {
                bot.send_message(chat_id, "Usage: /ai <template> | key=val | ...")
                    .await?;
                return Ok(());
            }
            let template = parts[0];
            let mut vars = std::collections::HashMap::new();
            for p in &parts[1..] {
                if let Some((k, v)) = p.split_once('=') {
                    vars.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
            match ai
                .generate(user_id, Some(user_id), template, vars, 300)
                .await
            {
                Ok(text) => {
                    bot.send_message(chat_id, text).await?;
                }
                Err(e) => {
                    bot.send_message(chat_id, format!("AI error: {}", e))
                        .await?;
                }
            }
        }
        Command::Setup(arg) => {
            let (chan, _) = split_channel_arg(&arg);
            let ch = chan
                .or_else(|| Some(arg.trim().to_string()))
                .filter(|s| !s.is_empty());
            if let Some(c) = ch {
                db.set_seller_channel(user_id, &c);
                db.upsert_seller(
                    user_id,
                    msg.from
                        .as_ref()
                        .and_then(|u| u.username.clone())
                        .as_deref(),
                    Some(&c),
                );
                bot.send_message(chat_id, format!("Setup complete. Default channel set to {}. Next: /planadd name | price | asset | days", c)).await?;
            } else {
                bot.send_message(chat_id, "Usage: /setup @yourchannel")
                    .await?;
            }
        }
        Command::BecomeSeller => {
            let uname = msg.from.as_ref().and_then(|u| u.username.clone());
            db.upsert_seller(user_id, uname.as_deref(), None);
            // provision web dashboard account if missing
            let username = uname
                .clone()
                .unwrap_or_else(|| format!("seller_{}", user_id));
            if db.get_web_account_by_username(&username).is_none() {
                let password = util::gen_token(12);
                if let Err(e) = db
                    .create_web_account(&username, &password, "seller", Some(user_id))
                    .await
                {
                    bot.send_message(
                        chat_id,
                        format!("Seller created, but web account failed: {}", e),
                    )
                    .await?;
                } else {
                    let url = cfg.web_url();
                    bot.send_message(chat_id, format!(
                        "You are now a seller. Use /setup @channel to link your channel.\n\nWeb dashboard login:\nURL: {}\nUsername: {}\nPassword: {}\nChange with /setwebpassword.",
                        url, username, password
                    )).await?;
                    return Ok(());
                }
            }
            bot.send_message(chat_id, "You are now a seller. Use /setup @channel to link your channel. Web account already exists; use /dashboard for login info.").await?;
        }
        Command::SetWebPassword(newpw) => {
            let uname = msg
                .from
                .as_ref()
                .and_then(|u| u.username.clone())
                .unwrap_or_else(|| format!("seller_{}", user_id));
            let newpw = newpw.trim().to_string();
            if newpw.len() < 6 {
                bot.send_message(chat_id, "Password must be at least 6 characters.")
                    .await?;
            } else if db.update_web_password(&uname, &newpw).await.is_ok() {
                bot.send_message(chat_id, "Web dashboard password updated.")
                    .await?;
            } else {
                bot.send_message(chat_id, "No web account found. Run /becomeseller first.")
                    .await?;
            }
        }
        Command::Dashboard => {
            let uname = msg
                .from
                .as_ref()
                .and_then(|u| u.username.clone())
                .unwrap_or_else(|| format!("seller_{}", user_id));
            let url = cfg.web_url();
            if db.get_web_account_by_username(&uname).is_some() {
                bot.send_message(chat_id, format!("Web dashboard: {}\nUsername: {}\n(Use /setwebpassword to change password.)", url, uname)).await?;
            } else {
                let password = util::gen_token(12);
                match db
                    .create_web_account(&uname, &password, "seller", Some(user_id))
                    .await
                {
                    Ok(_) => {
                        bot.send_message(
                            chat_id,
                            format!(
                                "Web dashboard created.\nURL: {}\nUsername: {}\nPassword: {}",
                                url, uname, password
                            ),
                        )
                        .await?;
                    }
                    Err(e) => {
                        bot.send_message(chat_id, format!("Failed to create web account: {}", e))
                            .await?;
                    }
                }
            }
        }
        Command::AddAdmin(arg) => {
            if role != roles::Role::Owner {
                return Ok(());
            }
            let parts: Vec<&str> = arg.split_whitespace().collect();
            if parts.is_empty() {
                bot.send_message(chat_id, "Usage: /addadmin <user_id> [username]")
                    .await?;
            } else {
                let uid: i64 = parts[0].parse().unwrap_or(0);
                if uid == 0 {
                    bot.send_message(chat_id, "Invalid user_id.").await?;
                } else {
                    let uname = parts.get(1).map(|s| s.to_string());
                    db.add_admin(uid, uname.as_deref());
                    bot.send_message(chat_id, format!("Admin added: {}", uid))
                        .await?;
                }
            }
        }
        Command::RemoveAdmin(arg) => {
            if role != roles::Role::Owner {
                return Ok(());
            }
            let uid: i64 = arg.trim().parse().unwrap_or(0);
            if uid == 0 {
                bot.send_message(chat_id, "Usage: /removeadmin <user_id>")
                    .await?;
            } else {
                db.remove_admin(uid);
                bot.send_message(chat_id, format!("Admin removed: {}", uid))
                    .await?;
            }
        }
        Command::Admins => {
            if role != roles::Role::Owner {
                return Ok(());
            }
            let admins = db.list_admins();
            let mut msg_text = String::from("Admins:\n");
            for a in admins {
                msg_text.push_str(&format!("- {}\n", a));
            }
            bot.send_message(chat_id, msg_text).await?;
        }
        Command::Sellers => {
            if role < roles::Role::Admin {
                return Ok(());
            }
            let sellers = db.list_sellers();
            let mut msg_text = String::from("Sellers:\n");
            for s in sellers {
                msg_text.push_str(&format!("- {}\n", s));
            }
            bot.send_message(chat_id, msg_text).await?;
        }
        Command::WebUsers => {
            if role != roles::Role::Owner {
                return Ok(());
            }
            let accounts = db.list_web_accounts();
            let mut msg_text = String::from("Web accounts:\n");
            for (u, r) in accounts {
                msg_text.push_str(&format!("- {} ({})\n", u, r));
            }
            bot.send_message(chat_id, msg_text).await?;
        }
    }
    Ok(())
}

/// Resolve `/buy <name>` candidates under the (seller_id, name) plan
/// namespace (2026-10-25 review HIGH: the old lookup was global by bare name,
/// leaking pricing across sellers and mis-resolving `/buy`). B5 fallback
/// semantics, mirroring `channel_of`:
///
/// 1. The caller's OWN namespace (seller_id = user_id) always wins — a seller
///    is never shadowed by, and can never resolve into, another seller's
///    same-name plan.
/// 2. The global namespace (seller_id = 0: legacy pre-multi-tenant rows and
///    admin-created catalogue entries) is resolvable only by Admin/Owner.
/// 3. Plain buyers (Role::User) fall back to the cross-seller active shop
///    view MINUS the global namespace — public-mode /buy keeps working
///    (README "Two modes"); ambiguous names still error exactly as the v1.2
///    record's deferred seller-disambiguation syntax intends.
/// 4. The Owner may additionally resolve any seller's entry by name
///    (superuser; the review's "owner can").
///
/// Returns (plans, products) in `list_plans`/`list_products` row shapes,
/// active rows only.
fn resolve_buy_candidates(
    db: &Db,
    role: roles::Role,
    user_id: i64,
    name: &str,
) -> (Vec<crate::db::PlanRow>, Vec<crate::db::ProductRow>) {
    let mut plans = db.find_plans_by_name(Some(user_id), name);
    let mut products: Vec<crate::db::ProductRow> = db
        .find_products_by_name(Some(user_id), name)
        .into_iter()
        .filter(|p| p.8 != 0)
        .collect();
    if plans.is_empty() && products.is_empty() {
        if role >= roles::Role::Admin {
            // Global (seller_id = 0) namespace: admin/owner-only.
            plans = db.find_plans_by_name(Some(0), name);
            products = db
                .find_products_by_name(Some(0), name)
                .into_iter()
                .filter(|p| p.8 != 0)
                .collect();
        }
        if role == roles::Role::User {
            // Public-mode shop view: any seller's active entry except the
            // global namespace, which stays admin/owner-only.
            plans = db
                .find_plans_by_name(None, name)
                .into_iter()
                .filter(|p| p.6 != 0)
                .collect();
            products = db
                .find_products_by_name(None, name)
                .into_iter()
                .filter(|p| p.8 != 0 && p.1 != 0)
                .collect();
        }
        if role == roles::Role::Owner && plans.is_empty() && products.is_empty() {
            // Owner superuser: may resolve any seller's entry by name.
            plans = db.find_plans_by_name(None, name);
            products = db
                .find_products_by_name(None, name)
                .into_iter()
                .filter(|p| p.8 != 0)
                .collect();
        }
    }
    (plans, products)
}

/// Resolve the target channel for a caller. Sellers may only operate on their
/// OWN channel (`seller_channel`); the global default fallback is reserved
/// for Admin/Owner, so a seller can never post/grant into a channel they do
/// not control via the default config.
fn channel_of(db: &Db, cfg: &Config, seller_id: Option<i64>, is_admin: bool) -> String {
    if let Some(sid) = seller_id
        && let Some(c) = db.seller_channel(sid)
    {
        return c;
    }
    if !is_admin {
        return String::new();
    }
    db.get_config("channel")
        .or_else(|| cfg.default_channel.clone())
        .unwrap_or_default()
}

fn split_channel_arg(input: &str) -> (Option<String>, &str) {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return (None, trimmed);
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let first = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim_start();
    let is_channel = first.starts_with('@') || first.parse::<i64>().map(|n| n < 0).unwrap_or(false);
    if is_channel {
        (Some(first.to_string()), rest)
    } else {
        (None, trimmed)
    }
}

fn parse_schedule_datetime(input: &str) -> Option<String> {
    use chrono::{DateTime, NaiveDateTime, Utc};
    if let Ok(dt) = DateTime::parse_from_rfc3339(input) {
        return Some(dt.with_timezone(&Utc).to_rfc3339());
    }
    let s = input.replace('T', " ");
    let fmts = ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"];
    for fmt in fmts {
        if let Ok(nd) = NaiveDateTime::parse_from_str(&s, fmt) {
            return Some(nd.and_utc().to_rfc3339());
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(input, "%Y-%m-%d") {
        let nd = d.and_hms_opt(0, 0, 0).unwrap();
        return Some(nd.and_utc().to_rfc3339());
    }
    None
}

/// Parse `/planadd name | price | asset | days` into
/// (name, price, asset, days); days must be in 1..=MAX_PLAN_DAYS.
pub(crate) fn parse_plan_add(input: &str) -> Result<(&str, &str, &str, i64), PlanAddError> {
    let parts: Vec<&str> = input.split('|').map(|s| s.trim()).collect();
    if parts.len() < 4 {
        return Err(PlanAddError::Usage);
    }
    match parts[3].parse::<i64>() {
        Ok(d) if (1..=MAX_PLAN_DAYS).contains(&d) => Ok((parts[0], parts[1], parts[2], d)),
        _ => Err(PlanAddError::Days(parts[3].to_string())),
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum PlanAddError {
    Usage,
    /// Raw days field that failed the 1..=MAX_PLAN_DAYS validation.
    Days(String),
}

/// Parse `/productadd name | price | asset | kind | delivery | [stock]` into
/// (name, price, asset, kind, delivery, stock); delivery/stock are None for
/// '-'/blank/omitted and stock must parse as i64 otherwise.
#[allow(clippy::type_complexity)] // mirrors the command's flat field list
pub(crate) fn parse_product_add(
    input: &str,
) -> Result<(&str, &str, &str, &str, Option<&str>, Option<i64>), ProductAddError> {
    let parts: Vec<&str> = input.split('|').map(|s| s.trim()).collect();
    if parts.len() < 5 || parts[0].is_empty() {
        return Err(ProductAddError::Usage);
    }
    let kind = parts[3];
    if !matches!(kind, "digital_link" | "digital_file" | "physical") {
        return Err(ProductAddError::Kind(kind.to_string()));
    }
    // Digital products must be deliverable at creation time: a payload-less
    // digital product would take money and then fail at delivery.
    let delivery = parts
        .get(4)
        .copied()
        .map(str::trim)
        .filter(|d| !d.is_empty() && *d != "-");
    if delivery.is_none() && kind != "physical" {
        return Err(ProductAddError::Delivery);
    }
    let stock = match parts.get(5).map(|s| s.trim()) {
        None | Some("") | Some("-") => None,
        Some(s) => s
            .parse::<i64>()
            .map(Some)
            .map_err(|_| ProductAddError::Stock(s.to_string()))?,
    };
    Ok((parts[0], parts[1], parts[2], kind, delivery, stock))
}

#[derive(Debug, PartialEq)]
pub(crate) enum ProductAddError {
    Usage,
    Kind(String),
    /// Digital kind given without a delivery payload.
    Delivery,
    Stock(String),
}

/// Parse `/orderset <order_id> <status> [tracking...]`, joining the tracking
/// words. The status whitelist itself is enforced by `Db::set_order_status`
/// (I/O) and is deliberately not checked here.
pub(crate) fn parse_order_set(input: &str) -> Option<(&str, &str, Option<String>)> {
    let parts: Vec<&str> = input.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }
    let tracking = (parts.len() > 2).then(|| parts[2..].join(" "));
    Some((parts[0], parts[1], tracking))
}

/// Parse `/gencodes plan | count | [days]`; count defaults to 1 and bad
/// numbers fall back silently (count -> 1, days -> None).
pub(crate) fn parse_gen_codes(input: &str) -> Option<(&str, usize, Option<i64>)> {
    let parts: Vec<&str> = input.split('|').map(|s| s.trim()).collect();
    if parts.is_empty() || parts[0].is_empty() {
        return None;
    }
    let count: usize = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
    let days: Option<i64> = parts.get(2).and_then(|s| s.parse().ok());
    Some((parts[0], count, days))
}

async fn send_menu(bot: &Bot, chat_id: teloxide::types::ChatId) -> HandlerResult {
    let kb = InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("Post", "menu:post"),
            InlineKeyboardButton::callback("Channel", "menu:channel"),
        ],
        vec![
            InlineKeyboardButton::callback("Crypto", "menu:crypto"),
            InlineKeyboardButton::callback("Help", "menu:help"),
        ],
    ]);
    bot.send_message(chat_id, "Menu:").reply_markup(kb).await?;
    Ok(())
}

async fn handle_crypto(
    bot: &Bot,
    chat_id: teloxide::types::ChatId,
    db: &Db,
    cfg: &Config,
    crypto: Option<Arc<CryptoPay>>,
    user_id: i64,
    arg: &str,
) -> HandlerResult {
    let arg = arg.trim();
    let parts: Vec<&str> = arg.split_whitespace().collect();
    if parts.is_empty() {
        bot.send_message(chat_id, "Usage: /crypto <me|balance|rates|invoices [status]|invoice <amount> <asset> [desc]|setup <token>|check <amount> <asset>>").await?;
        return Ok(());
    }
    match parts[0] {
        "setup" => {
            if parts.len() < 2 {
                bot.send_message(chat_id, "Usage: /crypto setup <token>")
                    .await?;
            } else {
                db.set_config("crypto_token", parts[1]);
                bot.send_message(chat_id, "Crypto Pay token saved.").await?;
            }
        }
        "me" => {
            let cp = crypto_or_load(db, cfg, crypto);
            match cp {
                Some(c) => match c.get_me().await {
                    Ok(v) => bot.send_message(chat_id, format!("App: {}", v)).await?,
                    Err(e) => bot.send_message(chat_id, format!("Error: {}", e)).await?,
                },
                None => { bot.send_message(chat_id, "No crypto token configured. Use /crypto setup <token> or set CRYPTO_PAY_TOKEN.").await? }
            };
        }
        "balance" => {
            let cp = crypto_or_load(db, cfg, crypto);
            match cp {
                Some(c) => match c.get_balance().await {
                    Ok(v) => bot.send_message(chat_id, format!("Balance: {}", v)).await?,
                    Err(e) => bot.send_message(chat_id, format!("Error: {}", e)).await?,
                },
                None => {
                    bot.send_message(chat_id, "No crypto token configured.")
                        .await?
                }
            };
        }
        "rates" => {
            let cp = crypto_or_load(db, cfg, crypto);
            match cp {
                Some(c) => match c.get_exchange_rates().await {
                    Ok(v) => bot.send_message(chat_id, format!("Rates: {}", v)).await?,
                    Err(e) => bot.send_message(chat_id, format!("Error: {}", e)).await?,
                },
                None => {
                    bot.send_message(chat_id, "No crypto token configured.")
                        .await?
                }
            };
        }
        "invoices" => {
            let status = parts.get(1).copied();
            let cp = crypto_or_load(db, cfg, crypto);
            match cp {
                Some(c) => match c.get_invoices(status).await {
                    Ok(v) => {
                        bot.send_message(chat_id, format!("Invoices: {}", v))
                            .await?
                    }
                    Err(e) => bot.send_message(chat_id, format!("Error: {}", e)).await?,
                },
                None => {
                    bot.send_message(chat_id, "No crypto token configured.")
                        .await?
                }
            };
        }
        "invoice" => {
            // /crypto invoice <amount> <asset> [desc...]
            if parts.len() < 3 {
                bot.send_message(
                    chat_id,
                    "Usage: /crypto invoice <amount> <asset> [description]",
                )
                .await?;
                return Ok(());
            }
            let amount = parts[1];
            let asset = parts[2];
            let desc = if parts.len() > 3 {
                Some(parts[3..].join(" "))
            } else {
                None
            };
            let cp = crypto_or_load(db, cfg, crypto);
            match cp {
                Some(c) => match c
                    .create_invoice(amount, asset, desc.as_deref(), Some("viewItem"), None)
                    .await
                {
                    Ok(v) => {
                        let inv_id = v
                            .get("invoice_id")
                            .map(|x| x.to_string())
                            .unwrap_or_default();
                        let pay_url = v.get("pay_url").and_then(|x| x.as_str()).unwrap_or("");
                        if !inv_id.is_empty() {
                            db.save_invoice(
                                &inv_id,
                                user_id,
                                amount,
                                asset,
                                desc.as_deref(),
                                "active",
                                Some(pay_url),
                                None,
                                // Bind the invoice to its creator so only that
                                // user (or Admin/Owner) can later complete it.
                                Some(user_id),
                                None,
                            );
                        }
                        bot.send_message(chat_id, format!("Invoice created:\n{}", v))
                            .await?
                    }
                    Err(e) => bot.send_message(chat_id, format!("Error: {}", e)).await?,
                },
                None => {
                    bot.send_message(chat_id, "No crypto token configured.")
                        .await?
                }
            };
        }
        "check" => {
            if parts.len() < 3 {
                bot.send_message(chat_id, "Usage: /crypto check <amount> <asset>")
                    .await?;
                return Ok(());
            }
            let cp = crypto_or_load(db, cfg, crypto);
            match cp {
                Some(c) => match c.create_check(parts[1], parts[2]).await {
                    Ok(v) => bot.send_message(chat_id, format!("Check: {}", v)).await?,
                    Err(e) => bot.send_message(chat_id, format!("Error: {}", e)).await?,
                },
                None => {
                    bot.send_message(chat_id, "No crypto token configured.")
                        .await?
                }
            };
        }
        _ => {
            bot.send_message(
                chat_id,
                "Unknown crypto subcommand. Use /crypto with no args for usage.",
            )
            .await?;
        }
    }
    Ok(())
}

fn crypto_or_load(db: &Db, cfg: &Config, crypto: Option<Arc<CryptoPay>>) -> Option<Arc<CryptoPay>> {
    if crypto.is_some() {
        return crypto;
    }
    let token = db
        .get_config("crypto_token")
        .or_else(|| cfg.crypto_token.clone());
    token.map(CryptoPay::new).map(Arc::new)
}

async fn setup_bot_profile(bot: &Bot) -> HandlerResult {
    use teloxide::types::BotCommand;
    let cmds = vec![
        BotCommand::new("help", "Show help"),
        BotCommand::new("menu", "Main menu"),
        BotCommand::new("channel", "Show target channel"),
        BotCommand::new("setchannel", "Set target channel"),
        BotCommand::new("post", "Post text to channel"),
        BotCommand::new("postphoto", "Post a photo"),
        BotCommand::new("edit", "Edit posted message"),
        BotCommand::new("delete", "Delete posted message"),
        BotCommand::new("pin", "Pin a message"),
        BotCommand::new("unpin", "Unpin all"),
        BotCommand::new("invite", "Create invite link"),
        BotCommand::new("setdesc", "Set channel description"),
        BotCommand::new("settitle", "Set channel title"),
        BotCommand::new("info", "Channel info"),
        BotCommand::new("crypto", "Crypto Pay commands"),
        BotCommand::new("plans", "List subscription plans"),
        BotCommand::new("planadd", "Add a plan"),
        BotCommand::new("plandel", "Remove a plan"),
        BotCommand::new("productadd", "Add a product"),
        BotCommand::new("products", "List my products"),
        BotCommand::new("productdel", "Deactivate a product"),
        BotCommand::new("shop", "Browse the shop"),
        BotCommand::new("buy", "Buy a plan or product"),
        BotCommand::new("paid", "Claim payment"),
        BotCommand::new("redeem", "Redeem activation code"),
        BotCommand::new("gencodes", "Generate activation codes"),
        BotCommand::new("codes", "List activation codes"),
        BotCommand::new("grant", "Grant access"),
        BotCommand::new("revoke", "Revoke access"),
        BotCommand::new("subs", "List subscriptions"),
        BotCommand::new("expirecheck", "Run expiry cleanup"),
        BotCommand::new("broadcast", "Broadcast to all users"),
        BotCommand::new("setup", "Easy seller setup"),
        BotCommand::new("becomeseller", "Become a seller"),
        BotCommand::new("ai", "AI generate content"),
    ];
    bot.set_my_commands(cmds).await?;
    bot.set_my_description()
        .description("Owner-only Telegram channel & crypto management agent.")
        .await?;
    bot.set_my_short_description()
        .short_description("Channel & crypto agent")
        .await?;
    Ok(())
}
async fn grant_access(
    bot: &Bot,
    channel: &str,
    _user_id: i64,
    days: i64,
) -> Result<String, String> {
    let cid = util::to_chat_id(channel);
    // Checked expiry math: an extreme `days` value must degrade to an
    // immediately-expiring link, never panic the request path.
    let expire = Duration::try_days(days)
        .and_then(|d| Utc::now().checked_add_signed(d))
        .unwrap_or_else(Utc::now);
    match bot
        .create_chat_invite_link(cid)
        .member_limit(1)
        .expire_date(expire)
        .await
    {
        Ok(link) => Ok(link.invite_link),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_add_parses() {
        assert_eq!(
            parse_plan_add("Gold | 9.9 | USDT | 30"),
            Ok(("Gold", "9.9", "USDT", 30))
        );
        assert_eq!(parse_plan_add("p|1|USDT|1"), Ok(("p", "1", "USDT", 1)));
        assert_eq!(
            parse_plan_add("p | 1 | USDT | 3650"),
            Ok(("p", "1", "USDT", MAX_PLAN_DAYS))
        );
    }

    #[test]
    fn plan_add_rejects() {
        // bad numbers and boundary days: 0 and 3651 are outside 1..=3650
        for bad in ["0", "3651", "abc", "", "-1"] {
            assert_eq!(
                parse_plan_add(&format!("p | 1 | USDT | {}", bad)),
                Err(PlanAddError::Days(bad.to_string()))
            );
        }
        assert_eq!(parse_plan_add("p | 1 | USDT"), Err(PlanAddError::Usage));
        assert_eq!(parse_plan_add(""), Err(PlanAddError::Usage));
    }

    #[test]
    fn product_add_valid() {
        assert_eq!(
            parse_product_add("VPN | 5 | USDT | digital_link | https://ex.co | 10"),
            Ok((
                "VPN",
                "5",
                "USDT",
                "digital_link",
                Some("https://ex.co"),
                Some(10)
            ))
        );
        // '-' delivery and '-' / blank / omitted stock all mean None
        assert_eq!(
            parse_product_add("Box | 3 | TON | physical | - | -"),
            Ok(("Box", "3", "TON", "physical", None, None))
        );
        assert_eq!(
            parse_product_add("Box | 3 | TON | physical | - |"),
            Ok(("Box", "3", "TON", "physical", None, None))
        );
    }

    #[test]
    fn product_add_invalid() {
        // missing delivery field / empty name
        assert_eq!(
            parse_product_add("VPN | 5 | USDT | digital_link"),
            Err(ProductAddError::Usage)
        );
        assert_eq!(
            parse_product_add(" | 5 | USDT | physical | d"),
            Err(ProductAddError::Usage)
        );
        assert_eq!(
            parse_product_add("n | 1 | USDT | ebook | d"),
            Err(ProductAddError::Kind("ebook".into()))
        );
        assert_eq!(
            parse_product_add("n | 1 | USDT | digital_file | -"),
            Err(ProductAddError::Delivery)
        );
        assert_eq!(
            parse_product_add("n | 1 | USDT | physical | d | x"),
            Err(ProductAddError::Stock("x".into()))
        );
    }

    #[test]
    fn order_set_parses() {
        assert_eq!(parse_order_set("A1 shipped"), Some(("A1", "shipped", None)));
        assert_eq!(
            parse_order_set("  A1   shipped  RR123 left depot "),
            Some(("A1", "shipped", Some("RR123 left depot".to_string())))
        );
        assert_eq!(parse_order_set("A1"), None);
        assert_eq!(parse_order_set(""), None);
    }

    #[test]
    fn gen_codes_parses() {
        assert_eq!(
            parse_gen_codes("basic | 5 | 30"),
            Some(("basic", 5, Some(30)))
        );
        assert_eq!(parse_gen_codes("basic"), Some(("basic", 1, None)));
        assert_eq!(parse_gen_codes("basic | x | y"), Some(("basic", 1, None)));
        assert_eq!(parse_gen_codes(" | 5 | 30"), None);
    }

    #[test]
    fn split_channel_arg_variants() {
        assert_eq!(
            split_channel_arg("@chan rest here"),
            (Some("@chan".into()), "rest here")
        );
        assert_eq!(
            split_channel_arg("-100123 rest"),
            (Some("-100123".into()), "rest")
        );
        assert_eq!(split_channel_arg("just text"), (None, "just text"));
        assert_eq!(split_channel_arg(""), (None, ""));
    }

    #[test]
    fn schedule_datetime_formats() {
        assert_eq!(
            parse_schedule_datetime("2026-01-02T03:04:05Z"),
            Some("2026-01-02T03:04:05+00:00".into())
        );
        assert_eq!(
            parse_schedule_datetime("2026-01-02 03:04"),
            Some("2026-01-02T03:04:00+00:00".into())
        );
        assert_eq!(
            parse_schedule_datetime("2026-01-02"),
            Some("2026-01-02T00:00:00+00:00".into())
        );
        assert_eq!(parse_schedule_datetime("tomorrow"), None);
    }

    // ---- /buy resolution over the (seller_id, name) plan namespace ----

    fn buy_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::new(dir.path().join("buy.db").to_str().unwrap()).unwrap();
        (dir, db)
    }

    #[test]
    fn buy_resolution_is_seller_scoped() {
        // Cross-seller plan isolation (2026-10-25 review HIGH): two sellers
        // may both define "pro" and each resolves only their own; a name
        // unique to another seller is invisible from the wrong namespace.
        let (_d, db) = buy_db();
        db.add_plan(1, "pro", "10", "USDT", 30, None);
        db.add_plan(2, "pro", "20", "USDT", 30, None);
        let (plans, products) = resolve_buy_candidates(&db, roles::Role::Seller, 1, "pro");
        assert!(products.is_empty());
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].1, "10");
        assert_eq!(plans[0].6, 1);
        let (plans2, _) = resolve_buy_candidates(&db, roles::Role::Seller, 2, "pro");
        assert_eq!(plans2[0].1, "20");
        db.add_plan(2, "solo", "5", "TON", 7, None);
        let (solo, _) = resolve_buy_candidates(&db, roles::Role::Seller, 1, "solo");
        assert!(solo.is_empty(), "seller 1 must not resolve seller 2's plan");
        // Same isolation for products (the /buy multiplex resolves both).
        db.add_product(2, "widget", "3", "TON", "digital_link", Some("l"), None)
            .unwrap();
        let (p, w) = resolve_buy_candidates(&db, roles::Role::Seller, 1, "widget");
        assert!(p.is_empty() && w.is_empty());
        let (_, w2) = resolve_buy_candidates(&db, roles::Role::Seller, 2, "widget");
        assert_eq!(w2.len(), 1);
    }

    #[test]
    fn buy_resolution_global_namespace_admin_owner_only() {
        // The global (seller_id = 0) namespace — legacy rows and admin-created
        // catalogue entries — is resolvable only by Admin/Owner (B5).
        let (_d, db) = buy_db();
        db.add_plan(0, "legacy", "3", "USDT", 30, None);
        let (p_user, _) = resolve_buy_candidates(&db, roles::Role::User, 7, "legacy");
        assert!(
            p_user.is_empty(),
            "plain users cannot resolve the global namespace"
        );
        let (p_seller, _) = resolve_buy_candidates(&db, roles::Role::Seller, 1, "legacy");
        assert!(
            p_seller.is_empty(),
            "sellers cannot resolve the global namespace"
        );
        let (p_admin, _) = resolve_buy_candidates(&db, roles::Role::Admin, 1, "legacy");
        assert_eq!(p_admin.len(), 1);
        assert_eq!(p_admin[0].6, 0);
        let (p_owner, _) = resolve_buy_candidates(&db, roles::Role::Owner, 1, "legacy");
        assert_eq!(p_owner.len(), 1);
    }

    #[test]
    fn buy_resolution_plain_user_shop_fallback_excludes_global() {
        // Public-mode /buy (README "Two modes"): a plain buyer falls back to
        // the cross-seller active shop view, minus the global namespace,
        // and the Owner may resolve any seller's entry by name.
        let (_d, db) = buy_db();
        db.add_plan(1, "vip", "9", "USDT", 30, None);
        db.add_plan(0, "gplan", "1", "USDT", 7, None);
        let (p, _) = resolve_buy_candidates(&db, roles::Role::User, 42, "vip");
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].6, 1);
        let (g, _) = resolve_buy_candidates(&db, roles::Role::User, 42, "gplan");
        assert!(
            g.is_empty(),
            "global namespace excluded from the shop fallback"
        );
        // Owner superuser ("owner can" in the review's test spec): any seller.
        let (o, _) = resolve_buy_candidates(&db, roles::Role::Owner, 999, "vip");
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].6, 1);
    }

    #[test]
    fn buy_resolution_ambiguous_name_still_reports_both() {
        // Seller-disambiguation syntax is deferred (v1.2 record): an
        // ambiguous name for a plain buyer returns multiple candidates so the
        // handler errors out instead of silently picking one.
        let (_d, db) = buy_db();
        db.add_plan(1, "dup", "1", "USDT", 7, None);
        db.add_plan(2, "dup", "2", "TON", 7, None);
        let (p, _) = resolve_buy_candidates(&db, roles::Role::User, 42, "dup");
        assert_eq!(p.len(), 2);
        // A seller with their own "dup" is never ambiguous: own namespace wins.
        db.add_plan(1, "dup", "3", "USDT", 7, None);
        let (p1, _) = resolve_buy_candidates(&db, roles::Role::Seller, 1, "dup");
        assert_eq!(p1.len(), 1);
        assert_eq!(p1[0].1, "3");
    }
}
