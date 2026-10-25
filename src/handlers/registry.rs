//! Runtime registry for plugin commands.
//!
//! The built-in bot commands are a compile-time enum ([`super::commands::Command`]);
//! this module adds an open registry so embedders (`crate::embed`) and hosts can
//! register extra `/commands` at startup without forking the dispatcher tree.
//! Registered commands are dispatched by the binary's tree after the built-in
//! `filter_command` branch and share the same role gate.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use teloxide::prelude::*;

use crate::{config::Config, db::Db, roles};

/// Everything a registered command handler receives. `args` is the message
/// text after the command word; `role` is the caller's resolved role.
pub struct CommandContext {
    pub bot: Bot,
    pub msg: Message,
    pub db: Arc<Db>,
    pub cfg: Config,
    pub args: String,
    pub role: roles::Role,
}

/// Boxed future returned by registered handlers; mirrors the binary's
/// `HandlerResult` error type.
pub type HandlerFuture =
    Pin<Box<dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send>>;

/// A registered command implementation.
pub type CommandHandler = Arc<dyn Fn(CommandContext) -> HandlerFuture + Send + Sync>;

/// One registrable `/command`: its name, help text, minimum role and handler.
#[derive(Clone)]
pub struct CommandSpec {
    pub name: String,
    pub description: String,
    pub min_role: roles::Role,
    pub handler: CommandHandler,
}

/// The set of registered commands; owned by the dispatcher/embedded app.
#[derive(Default)]
pub struct CommandRegistry {
    specs: Vec<CommandSpec>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self { specs: Vec::new() }
    }

    /// Register `spec`. Errors on a duplicate name, or on a name that is
    /// empty, contains whitespace, or contains `/` (it would never match a
    /// parsed `/command` word).
    pub fn register(&mut self, spec: CommandSpec) -> Result<(), String> {
        if spec.name.is_empty() || spec.name.chars().any(|c| c.is_whitespace() || c == '/') {
            return Err(format!(
                "invalid command name '{}': must be non-empty, without whitespace or '/'",
                spec.name
            ));
        }
        if self.get(&spec.name).is_some() {
            return Err(format!("command '/{}' is already registered", spec.name));
        }
        self.specs.push(spec);
        Ok(())
    }

    /// Look up a command by name (without the leading `/`).
    pub fn get(&self, name: &str) -> Option<CommandSpec> {
        self.specs.iter().find(|s| s.name == name).cloned()
    }

    /// `/name — description` lines (one per registered command, each
    /// newline-terminated) for appending to `/help`.
    pub fn descriptions(&self) -> String {
        self.specs
            .iter()
            .map(|s| format!("/{} — {}\n", s.name, s.description))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn noop(_ctx: CommandContext) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    fn spec(name: &str, description: &str) -> CommandSpec {
        CommandSpec {
            name: name.to_string(),
            description: description.to_string(),
            min_role: roles::Role::User,
            handler: Arc::new(|ctx: CommandContext| Box::pin(noop(ctx)) as HandlerFuture),
        }
    }

    #[test]
    fn register_duplicate_is_err() {
        let mut reg = CommandRegistry::new();
        reg.register(spec("echo", "Echo text")).unwrap();
        let err = reg.register(spec("echo", "Echo text again")).unwrap_err();
        assert!(err.contains("already registered"), "{err}");
    }

    #[test]
    fn register_rejects_whitespace_slash_and_empty_names() {
        let mut reg = CommandRegistry::new();
        for bad in ["", "a b", "/echo", "echo/", "a\nb"] {
            assert!(
                reg.register(spec(bad, "desc")).is_err(),
                "{bad:?} must be rejected"
            );
        }
        assert!(reg.get("echo").is_none());
    }

    #[test]
    fn lookup_finds_registered_and_rejects_missing() {
        let mut reg = CommandRegistry::new();
        reg.register(spec("echo", "Echo text")).unwrap();
        reg.register(spec("status", "Show status")).unwrap();
        assert_eq!(reg.get("echo").unwrap().name, "echo");
        assert_eq!(reg.get("status").unwrap().description, "Show status");
        assert!(reg.get("missing").is_none());
    }

    #[test]
    fn descriptions_lists_all_commands() {
        let mut reg = CommandRegistry::new();
        reg.register(spec("echo", "Echo text")).unwrap();
        reg.register(spec("status", "Show status")).unwrap();
        assert_eq!(
            reg.descriptions(),
            "/echo — Echo text\n/status — Show status\n"
        );
    }
}
