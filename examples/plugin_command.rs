//! Register a custom bot command (`/echo`) alongside the built-ins.
//!
//! Compile-level demonstration: build the app with the command registered,
//! print what landed in the registry, and exit (nothing is served).
//!
//! Run with: `cargo run --example plugin_command`

use std::sync::Arc;

use telegram_agent::{
    embed::AgentAppBuilder,
    handlers::registry::{CommandContext, HandlerFuture},
    roles,
};
use teloxide::prelude::*;

async fn echo_handler(ctx: CommandContext) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    ctx.bot.send_message(ctx.msg.chat.id, ctx.args).await?;
    Ok(())
}

fn echo_spec() -> telegram_agent::handlers::registry::CommandSpec {
    telegram_agent::handlers::registry::CommandSpec {
        name: "echo".to_string(),
        description: "Echo the given text back".to_string(),
        min_role: roles::Role::User,
        handler: Arc::new(|ctx: CommandContext| Box::pin(echo_handler(ctx)) as HandlerFuture),
    }
}

#[tokio::main]
async fn main() {
    let app = AgentAppBuilder::new()
        .db_path("data/plugin-example.db")
        .command(echo_spec())
        .build()
        .await
        .expect("build agent app");

    println!(
        "plugin commands registered:\n{}",
        app.registry().descriptions()
    );
}
