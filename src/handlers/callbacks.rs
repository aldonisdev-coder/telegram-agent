use teloxide::prelude::*;
use teloxide::types::CallbackQuery;

type HandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

pub async fn callback_handler(bot: Bot, q: CallbackQuery) -> HandlerResult {
    bot.answer_callback_query(q.id.clone()).await?;
    if let Some(data) = q.data.as_deref()
        && let Some(msg) = q.regular_message()
    {
        let chat_id = msg.chat.id;
        let text = match data {
            "menu:post" => "Use /post <text> or /postphoto.",
            "menu:channel" => "Use /channel, /info, /setchannel.",
            "menu:crypto" => "Use /crypto ...",
            "menu:help" => "Use /help for full command list.",
            _ => "Unknown action.",
        };
        bot.send_message(chat_id, text).await?;
    }
    Ok(())
}
