use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Clone)]
pub struct Config {
    pub bot_token: String,
    pub owner_id: i64,
    pub db_path: String,
    pub default_channel: Option<String>,
    pub crypto_token: Option<String>,
    pub ai_enabled: bool,
    pub ai_provider: String,
    pub ai_base_url: String,
    pub ai_api_key: Option<String>,
    pub ai_model: String,
    pub ai_daily_token_budget: Option<i64>,
    /// Static bearer token for the MCP endpoint. None/empty = endpoint fully
    /// disabled (secure default OFF) — see `mcp_enabled`.
    pub mcp_owner_token: Option<String>,
}

pub const APP_DIR_NAME: &str = "telegram-agent";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DesktopOs {
    Windows,
    MacOs,
    Unix,
}

/// Pure data-directory resolution, testable without touching the environment.
///
/// Priority: explicit override > portable mode (a `.env`, `data/`, or
/// `schema.sql` next to the binary/cwd — repo and unzipped-bundle layouts) >
/// the platform convention dir. Desktop users who double-click a distributed
/// binary get the platform dir, never a database dropped in a random cwd.
fn resolve_data_dir(
    os: DesktopOs,
    override_dir: Option<&str>,
    portable_marker: bool,
    home: Option<&str>,
    appdata: Option<&str>,
    xdg_data: Option<&str>,
) -> PathBuf {
    if let Some(dir) = override_dir.filter(|s| !s.trim().is_empty()) {
        return PathBuf::from(dir);
    }
    if portable_marker {
        return PathBuf::from("data");
    }
    let base = match os {
        DesktopOs::Windows => appdata.filter(|s| !s.trim().is_empty()).map(PathBuf::from),
        DesktopOs::MacOs => home
            .filter(|s| !s.trim().is_empty())
            .map(|h| PathBuf::from(h).join("Library").join("Application Support")),
        DesktopOs::Unix => xdg_data
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                home.filter(|s| !s.trim().is_empty())
                    .map(|h| PathBuf::from(h).join(".local").join("share"))
            }),
    };
    base.unwrap_or_else(|| PathBuf::from("data"))
        .join(APP_DIR_NAME)
}

/// Where per-user app data (agent.db, fallback .env) lives on this machine.
/// Creates the directory; falls back to `./data` if creation is impossible
/// (read-only home), which is still a usable cwd-relative location.
pub fn data_dir() -> PathBuf {
    let os = if cfg!(target_os = "windows") {
        DesktopOs::Windows
    } else if cfg!(target_os = "macos") {
        DesktopOs::MacOs
    } else {
        DesktopOs::Unix
    };
    let portable_marker = PathBuf::from("data").exists()
        || PathBuf::from(".env").exists()
        || PathBuf::from("schema.sql").exists();
    let dir = resolve_data_dir(
        os,
        env::var("DATABASE_DIR").ok().as_deref(),
        portable_marker,
        env::var("HOME").ok().as_deref(),
        env::var("APPDATA").ok().as_deref(),
        env::var("XDG_DATA_HOME").ok().as_deref(),
    );
    if fs::create_dir_all(&dir).is_err() {
        return PathBuf::from("data");
    }
    dir
}

/// Load `.env` from the cwd first; if absent, from the platform data dir
/// (macOS Finder and Windows Explorer launch the app with an unrelated cwd,
/// so the bundled `.env` beside the data dir must still be honored).
/// Idempotent: existing variables are never overridden.
pub fn load_dotenv() {
    if dotenvy::dotenv().is_ok() {
        return;
    }
    let _ = dotenvy::from_path(data_dir().join(".env"));
}

/// Normalize a `WEB_BASE_PATH` value: trim whitespace, strip trailing `/`s,
/// map `"/"` (and anything empty) to `""`, and require a leading `/` on
/// anything that remains (otherwise `""`). Pure so the rules are testable
/// without touching process env.
fn normalize_web_base_path(raw: &str) -> String {
    let s = raw.trim().trim_end_matches('/');
    if s.is_empty() {
        return String::new();
    }
    if s.starts_with('/') {
        s.to_string()
    } else {
        String::new()
    }
}

impl Config {
    pub fn is_configured(&self) -> bool {
        !self.bot_token.trim().is_empty() && self.owner_id != 0
    }

    /// Whether the MCP endpoint accepts requests at all. Unset/empty token =
    /// OFF regardless of what a caller presents, so the endpoint can never be
    /// reached by accident on a default install.
    pub fn mcp_enabled(&self) -> bool {
        self.mcp_owner_token
            .as_deref()
            .is_some_and(|t| !t.is_empty())
    }

    pub fn web_port(&self) -> u16 {
        env::var("WEB_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8080)
    }

    /// Bind address for the web dashboard. Secure-by-default `127.0.0.1` for
    /// desktop use; servers/Docker set `WEB_BIND=0.0.0.0` explicitly.
    pub fn web_bind(&self) -> String {
        env::var("WEB_BIND")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "127.0.0.1".to_string())
    }

    /// Open the dashboard in the system browser at startup. Defaults to auto
    /// on desktop platforms (Windows/macOS); servers opt in with OPEN_BROWSER=1.
    pub fn open_browser(&self) -> bool {
        match env::var("OPEN_BROWSER").ok().as_deref().map(str::trim) {
            Some(v) if v == "1" || v.eq_ignore_ascii_case("true") => true,
            Some(v) if v == "0" || v.eq_ignore_ascii_case("false") => false,
            _ => cfg!(windows) || cfg!(target_os = "macos"),
        }
    }

    pub fn web_username(&self) -> String {
        env::var("WEB_USERNAME")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "owner".to_string())
    }

    /// Base path the dashboard is served under (reverse-proxy / embedded
    /// hosting), read from `WEB_BASE_PATH`. Empty means the root. See
    /// [`normalize_web_base_path`] for the exact rules.
    pub fn web_base_path(&self) -> String {
        normalize_web_base_path(env::var("WEB_BASE_PATH").ok().as_deref().unwrap_or(""))
    }

    pub fn web_url(&self) -> String {
        if let Ok(u) = env::var("WEB_URL")
            && !u.trim().is_empty()
        {
            return u.trim().trim_end_matches('/').to_string();
        }
        format!(
            "http://localhost:{}{}",
            self.web_port(),
            self.web_base_path()
        )
    }

    pub fn from_env() -> Self {
        load_dotenv();
        let bot_token = env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default();
        let owner_id = env::var("OWNER_ID")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let db_path = match env::var("DATABASE_URL") {
            Ok(url) => url.trim_start_matches("sqlite:").to_string(),
            Err(_) => data_dir().join("agent.db").to_string_lossy().into_owned(),
        };
        let default_channel = env::var("DEFAULT_CHANNEL").ok().filter(|s| !s.is_empty());
        let crypto_token = env::var("CRYPTO_PAY_TOKEN").ok().filter(|s| !s.is_empty());
        let ai_enabled = env::var("AI_ENABLED")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let ai_provider = env::var("AI_PROVIDER").unwrap_or_else(|_| "openai".to_string());
        let ai_base_url =
            env::var("AI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
        let ai_api_key = env::var("AI_API_KEY").ok().filter(|s| !s.is_empty());
        let ai_model = env::var("AI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());
        let ai_daily_token_budget = env::var("AI_DAILY_TOKEN_BUDGET")
            .ok()
            .and_then(|s| s.parse().ok());
        let mcp_owner_token = env::var("MCP_OWNER_TOKEN").ok().filter(|s| !s.is_empty());
        Self {
            bot_token,
            owner_id,
            db_path,
            default_channel,
            crypto_token,
            ai_enabled,
            ai_provider,
            ai_base_url,
            ai_api_key,
            ai_model,
            ai_daily_token_budget,
            mcp_owner_token,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn data_dir_override_wins() {
        for os in [DesktopOs::Windows, DesktopOs::MacOs, DesktopOs::Unix] {
            assert_eq!(
                resolve_data_dir(
                    os,
                    Some("/custom"),
                    true,
                    Some("/h"),
                    Some("/a"),
                    Some("/x")
                ),
                p("/custom")
            );
        }
    }

    #[test]
    fn data_dir_portable_mode() {
        for os in [DesktopOs::Windows, DesktopOs::MacOs, DesktopOs::Unix] {
            assert_eq!(
                resolve_data_dir(os, None, true, Some("/h"), Some("/a"), Some("/x")),
                p("data")
            );
        }
    }

    #[test]
    fn data_dir_windows_uses_appdata() {
        assert_eq!(
            resolve_data_dir(
                DesktopOs::Windows,
                None,
                false,
                Some("/h"),
                Some("C:/Users/u/AppData/Roaming"),
                None
            ),
            p("C:/Users/u/AppData/Roaming").join(APP_DIR_NAME)
        );
    }

    #[test]
    fn data_dir_macos_uses_application_support() {
        assert_eq!(
            resolve_data_dir(
                DesktopOs::MacOs,
                None,
                false,
                Some("/Users/u"),
                Some("/a"),
                Some("/x")
            ),
            p("/Users/u/Library/Application Support").join(APP_DIR_NAME)
        );
    }

    #[test]
    fn data_dir_unix_prefers_xdg() {
        assert_eq!(
            resolve_data_dir(
                DesktopOs::Unix,
                None,
                false,
                Some("/home/u"),
                None,
                Some("/home/u/.local/data")
            ),
            p("/home/u/.local/data").join(APP_DIR_NAME)
        );
    }

    #[test]
    fn data_dir_unix_falls_back_to_home_share() {
        assert_eq!(
            resolve_data_dir(DesktopOs::Unix, None, false, Some("/home/u"), None, None),
            p("/home/u/.local/share").join(APP_DIR_NAME)
        );
    }

    #[test]
    fn data_dir_last_resort_is_cwd() {
        for os in [DesktopOs::Windows, DesktopOs::MacOs, DesktopOs::Unix] {
            assert_eq!(
                resolve_data_dir(os, None, false, None, None, None),
                p("data").join(APP_DIR_NAME)
            );
        }
    }

    #[test]
    fn data_dir_blank_override_is_ignored() {
        assert_eq!(
            resolve_data_dir(DesktopOs::Unix, Some("  "), true, None, None, None),
            p("data")
        );
    }

    #[test]
    fn base_path_empty_and_root_normalize_to_empty() {
        for raw in ["", " ", "\t", "/", "//", " / "] {
            assert_eq!(normalize_web_base_path(raw), "", "{raw:?}");
        }
    }

    #[test]
    fn base_path_trims_and_strips_trailing_slashes() {
        assert_eq!(normalize_web_base_path("/agent"), "/agent");
        assert_eq!(normalize_web_base_path("/agent/"), "/agent");
        assert_eq!(normalize_web_base_path("/agent///"), "/agent");
        assert_eq!(normalize_web_base_path("  /agent/  "), "/agent");
        assert_eq!(normalize_web_base_path("/a/b/"), "/a/b");
    }

    #[test]
    fn base_path_without_leading_slash_is_rejected() {
        assert_eq!(normalize_web_base_path("agent"), "");
        assert_eq!(normalize_web_base_path("agent/"), "");
        assert_eq!(normalize_web_base_path("a/b"), "");
    }
}
