//! Product delivery: the post-payment fulfillment DM.
//!
//! Single-sourced so the three grant sites (background granter in the bin,
//! `/paid`, and the claim API) send byte-identical deliveries. Delivery is
//! strictly a private message to the buyer — payloads are never rendered in
//! channels or group chats.

/// Deliver a purchased product to the buyer's private chat.
///
/// - `digital_link` / `digital_file`: sends `🎁 {name}\n{delivery}` where
///   `delivery` is the link, text, or Telegram `file_id` stored on the
///   product. The payload is required: a product without one is a seller
///   configuration error and surfaces as `Err` (never an empty delivery).
/// - `physical`: sends the order receipt ("order received, seller will
///   ship"); nothing is shipped by the platform.
/// - any other kind is rejected: new kinds must be wired deliberately, never
///   silently delivered as the wrong thing.
///
/// Never fails the surrounding grant flow fatally: callers decide whether a
/// delivery error is retried; the grant itself is already durable.
pub async fn deliver(
    bot: &teloxide::prelude::Bot,
    user_id: i64,
    kind: &str,
    name: &str,
    delivery: Option<&str>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use teloxide::prelude::Requester as _;
    let chat = teloxide::types::ChatId(user_id);
    match kind {
        // A file_id payload re-sends the actual document; anything else
        // (http(s) link or plain text) goes out as a message.
        "digital_file" => {
            let payload = delivery.filter(|d| !d.trim().is_empty()).ok_or_else(|| {
                format!(
                    "product '{}' ({}) has no delivery payload configured",
                    name, kind
                )
            })?;
            if payload.starts_with("http://")
                || payload.starts_with("https://")
                || !payload
                    .trim_start_matches('-')
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric())
            {
                bot.send_message(chat, format!("🎁 {name}\n{payload}"))
                    .await?;
            } else {
                bot.send_document(
                    chat,
                    teloxide::types::InputFile::file_id(teloxide::types::FileId(
                        payload.to_string(),
                    )),
                )
                .await?;
            }
        }
        "digital_link" => {
            let payload = delivery.filter(|d| !d.trim().is_empty()).ok_or_else(|| {
                format!(
                    "product '{}' ({}) has no delivery payload configured",
                    name, kind
                )
            })?;
            bot.send_message(chat, format!("🎁 {name}\n{payload}"))
                .await?;
        }
        "physical" => {
            bot.send_message(
                chat,
                format!(
                    "Order received for '{}'. The seller will ship it to you soon.\nYou can track it with /orders once the seller ships.",
                    name
                ),
            )
            .await?;
        }
        other => {
            return Err(format!("unknown product kind '{}'", other).into());
        }
    }
    Ok(())
}
