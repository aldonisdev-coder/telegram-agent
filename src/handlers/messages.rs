use crate::{config::Config, db::Db, dialogue::State, util};
use std::sync::Arc;
use teloxide::{prelude::*, types::InputFile};

type HandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

pub async fn awaiting_photo_handler(
    bot: Bot,
    msg: Message,
    dialogue: Dialogue<State, teloxide::dispatching::dialogue::ErasedStorage<State>>,
    db: Arc<Db>,
    cfg: Config,
) -> HandlerResult {
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);
    if !db.is_admin(user_id, cfg.owner_id) {
        return Ok(());
    }

    // If user sends /cancel
    if let Some(text) = msg.text()
        && text == "/cancel"
    {
        dialogue.exit().await?;
        bot.send_message(msg.chat.id, "Cancelled.").await?;
        return Ok(());
    }

    let ch = db
        .get_config("pending_channel")
        .or_else(|| db.get_config("channel"))
        .or_else(|| cfg.default_channel.clone())
        .unwrap_or_default();
    if ch.is_empty() {
        bot.send_message(
            msg.chat.id,
            "No target channel configured. Use /setchannel first.",
        )
        .await?;
        dialogue.exit().await?;
        return Ok(());
    }

    if let Some(photos) = msg.photo() {
        // largest photo
        if let Some(p) = photos.last() {
            let file_id = p.file.id.clone();
            let caption = msg
                .caption()
                .map(|s| s.to_string())
                .or_else(|| db.get_config("pending_caption"));
            let mut req = bot.send_photo(util::to_chat_id(&ch), InputFile::file_id(file_id));
            if let Some(c) = caption.clone() {
                req = req.caption(c);
            }
            match req.await {
                Ok(sent) => {
                    let _ = db.log_post(user_id, &ch, sent.id.0, "photo", caption.as_deref());
                    bot.send_message(
                        msg.chat.id,
                        format!("Photo posted. message_id={}", sent.id.0),
                    )
                    .await?;
                }
                Err(e) => {
                    bot.send_message(msg.chat.id, format!("Failed: {}", e))
                        .await?;
                }
            }
            db.set_config("pending_caption", "");
            dialogue.exit().await?;
            return Ok(());
        }
    }

    bot.send_message(msg.chat.id, "Please send a photo (or /cancel).")
        .await?;
    Ok(())
}

pub async fn crypto_invoice_handler(
    bot: Bot,
    msg: Message,
    dialogue: Dialogue<State, teloxide::dispatching::dialogue::ErasedStorage<State>>,
    _db: Arc<Db>,
    _cfg: Config,
) -> HandlerResult {
    // Minimal: cancel support; wizard optional. Exit on any input.
    if let Some(text) = msg.text()
        && text == "/cancel"
    {
        dialogue.exit().await?;
        bot.send_message(msg.chat.id, "Cancelled.").await?;
        return Ok(());
    }
    dialogue.exit().await?;
    Ok(())
}
