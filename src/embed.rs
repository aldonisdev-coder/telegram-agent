//! Embedding API: build the whole agent (dashboard + API + plugin-command
//! registry) as a library component of another axum host.
//!
//! ```no_run
//! use telegram_agent::embed::AgentAppBuilder;
//!
//! # async fn demo() -> Result<(), String> {
//! let app = AgentAppBuilder::new()
//!     .base_path("/agent")
//!     .db_path("data/host.db")
//!     .build()
//!     .await?;
//! let router = app.router(); // mount this under your host Router
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use teloxide::Bot;

use crate::{
    api,
    config::Config,
    db::Db,
    handlers::registry::{CommandRegistry, CommandSpec},
    util, web,
    web::WebState,
};

/// Builder for [`AgentApp`]. Starts from the ambient environment
/// ([`Config::from_env`]); every aspect can be overridden before `build`.
#[derive(Clone)]
pub struct AgentAppBuilder {
    cfg: Config,
    db_path_override: Option<String>,
    base_override: Option<String>,
    commands: Vec<CommandSpec>,
}

impl AgentAppBuilder {
    /// A builder configured from the environment (`.env` included).
    pub fn new() -> Self {
        Self {
            cfg: Config::from_env(),
            db_path_override: None,
            base_override: None,
            commands: Vec::new(),
        }
    }

    /// Replace the whole configuration.
    pub fn config(mut self, cfg: Config) -> Self {
        self.cfg = cfg;
        self
    }

    /// Override the database path (takes precedence over the config's).
    pub fn db_path(mut self, path: impl Into<String>) -> Self {
        self.db_path_override = Some(path.into());
        self
    }

    /// Override the web base path (takes precedence over `WEB_BASE_PATH`).
    pub fn base_path(mut self, base: impl Into<String>) -> Self {
        self.base_override = Some(base.into());
        self
    }

    /// Register a plugin command; duplicate or malformed names fail `build`.
    pub fn command(mut self, spec: CommandSpec) -> Self {
        self.commands.push(spec);
        self
    }

    /// Open the database (creating its parent directory when needed), seed the
    /// owner web account exactly like the binary does, and assemble the app's
    /// state and command registry.
    pub async fn build(self) -> Result<AgentApp, String> {
        let cfg = self.cfg;
        let db_path = self.db_path_override.unwrap_or_else(|| cfg.db_path.clone());
        if let Some(parent) = std::path::Path::new(&db_path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let db = Db::new(&db_path).map_err(|e| format!("failed to open db at {db_path}: {e}"))?;
        let db = Arc::new(db);

        // Seed owner web account (idempotent) only when configured — mirrors
        // main.rs, including the generated-password-once-on-stderr behavior.
        if cfg.is_configured() {
            let uname = cfg.web_username();
            let pw = std::env::var("WEB_PASSWORD")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| {
                    let pw = util::gen_token(16);
                    eprintln!("Generated web dashboard password (WEB_PASSWORD was unset): {pw}");
                    pw
                });
            if db.get_web_account_by_username(&uname).is_none()
                && let Err(e) = db.create_web_account(&uname, &pw, "owner", None)
            {
                tracing::warn!("failed to seed owner web account: {}", e);
            }
        }

        let bot = if cfg.is_configured() {
            Bot::new(cfg.bot_token.clone())
        } else {
            Bot::new("0:setup-placeholder".to_string())
        };

        let mut registry = CommandRegistry::new();
        for spec in self.commands {
            registry
                .register(spec)
                .map_err(|e| format!("plugin command rejected: {e}"))?;
        }
        let registry = Arc::new(registry);

        let base = self.base_override.unwrap_or_else(|| cfg.web_base_path());
        let state = WebState {
            db,
            cfg,
            bot,
            login_attempts: Arc::new(Mutex::new(HashMap::new())),
            base_path: base,
        };
        Ok(AgentApp { state, registry })
    }
}

impl Default for AgentAppBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// An assembled, ready-to-serve agent: shared [`WebState`] plus the plugin
/// command registry. Cheap to clone-by-ref via its routers.
pub struct AgentApp {
    state: WebState,
    registry: Arc<CommandRegistry>,
}

impl AgentApp {
    /// The shared state behind all routers (db, config, bot, base path).
    pub fn state(&self) -> &WebState {
        &self.state
    }

    /// The dashboard router, already mounted under the configured base path.
    pub fn web_router(&self) -> axum::Router {
        web::mount(&self.state.base_path, web::router(self.state.clone()))
    }

    /// The JSON API router, already mounted under the configured base path.
    pub fn api_router(&self) -> axum::Router {
        web::mount(&self.state.base_path, api::router(self.state.clone()))
    }

    /// Dashboard + JSON API merged, already mounted under the base path —
    /// nest this into a host router, or serve it directly at the root.
    pub fn router(&self) -> axum::Router {
        web::mount(
            &self.state.base_path,
            web::router(self.state.clone()).merge(api::router(self.state.clone())),
        )
    }

    /// The plugin command registry (for host-side inspection or `/help`).
    pub fn registry(&self) -> Arc<CommandRegistry> {
        self.registry.clone()
    }
}
