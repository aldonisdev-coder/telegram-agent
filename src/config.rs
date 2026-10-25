use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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

// ---- .env writer (v1.3.x: extracted from the setup form so setup AND the
// owner settings page share one writer) ----

/// `.env` keys owned by the setup/settings forms: each write rewrites exactly
/// these. Every other line in an existing `.env` (WEB_BIND, WEB_URL,
/// WEB_BASE_PATH, OPEN_BROWSER, ...) is preserved verbatim so a re-run never
/// drops deployment settings it knows nothing about.
pub const SETUP_ENV_KEYS: [&str; 9] = [
    "TELEGRAM_BOT_TOKEN",
    "OWNER_ID",
    "DATABASE_URL",
    "DEFAULT_CHANNEL",
    "CRYPTO_PAY_TOKEN",
    "WEB_USERNAME",
    "WEB_PASSWORD",
    "WEB_PORT",
    "RUST_LOG",
];

/// Pure path resolution for the `.env` writer, testable without touching the
/// process environment. An explicit `ENV_FILE` wins even when that file does
/// not exist yet (a blank value is ignored and falls through); then an
/// existing cwd `.env`; then an existing data-dir `.env`; a fresh file is
/// created in the cwd. Mirrors `load_dotenv`'s lookup, so the file written is
/// the file read on the next start.
fn resolve_env_path(explicit: Option<&str>, cwd_env: &Path, data_env: &Path) -> PathBuf {
    if let Some(p) = explicit.filter(|s| !s.trim().is_empty()) {
        return PathBuf::from(p);
    }
    if cwd_env.exists() || !data_env.exists() {
        cwd_env.to_path_buf()
    } else {
        data_env.to_path_buf()
    }
}

/// Write the setup/settings `KEY=value` pairs into the active `.env`,
/// preserving every line whose key is outside [`SETUP_ENV_KEYS`] verbatim
/// (comments and blank lines are dropped, matching the original setup
/// writer). `updates` are written first in the given order, preserved lines
/// follow. Best-effort `0600` permissions on unix — secrets live here.
/// Reading an existing file that cannot be read fails closed (Err) rather
/// than clobbering unknown content. Returns the path written.
pub fn write_env_file(updates: &[(String, String)]) -> Result<PathBuf, io::Error> {
    let explicit = env::var("ENV_FILE").ok();
    let data_env = data_dir().join(".env");
    let path = resolve_env_path(explicit.as_deref(), Path::new(".env"), &data_env);
    write_env_file_at(&path, updates)
}

/// Core of [`write_env_file`] against an explicit path (no environment
/// reads), so the merge rules and permission handling are testable in
/// isolation. The caller owns choosing the path via [`resolve_env_path`].
fn write_env_file_at(path: &Path, updates: &[(String, String)]) -> Result<PathBuf, io::Error> {
    let mut preserved: Vec<String> = Vec::new();
    if path.exists() {
        let existing = fs::read_to_string(path).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("cannot read existing {}: {e}", path.display()),
            )
        })?;
        for line in existing.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            // Key = text before the first `=`, top level only.
            match line.split('=').next() {
                Some(key) if SETUP_ENV_KEYS.contains(&key) => continue,
                _ => preserved.push(line.to_string()),
            }
        }
    }
    let mut content = String::new();
    for (k, v) in updates {
        content.push_str(&format!("{k}={v}\n"));
    }
    if !preserved.is_empty() {
        content.push_str(&preserved.join("\n"));
        content.push('\n');
    }
    fs::write(path, content)
        .map_err(|e| io::Error::new(e.kind(), format!("cannot write {}: {e}", path.display())))?;
    // best-effort permissions 0600
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(path.to_path_buf())
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

    // ---- .env writer ----

    #[test]
    fn env_writer_preserves_foreign_keys_and_rewrites_owned_ones() {
        let dir = tempfile::tempdir().unwrap();
        let env = dir.path().join(".env");
        std::fs::write(
            &env,
            "# deployment settings\nWEB_BIND=0.0.0.0\nTELEGRAM_BOT_TOKEN=old\n\nWEB_URL=https://example.com\n",
        )
        .unwrap();
        let written = write_env_file_at(
            &env,
            &[
                ("TELEGRAM_BOT_TOKEN".to_string(), "new".to_string()),
                ("WEB_PASSWORD".to_string(), "hunter2".to_string()),
            ],
        )
        .unwrap();
        assert_eq!(written, env);
        let content = std::fs::read_to_string(&env).unwrap();
        // Owned keys are rewritten, updates first in the given order.
        assert!(content.starts_with("TELEGRAM_BOT_TOKEN=new\nWEB_PASSWORD=hunter2\n"));
        assert!(!content.contains("old"), "stale owned value must go");
        // Foreign keys are preserved verbatim (in the preserved tail).
        assert!(content.contains("WEB_BIND=0.0.0.0\n"));
        assert!(content.contains("WEB_URL=https://example.com\n"));
        // Comments and blank lines are dropped (original setup-writer rules).
        assert!(!content.contains("deployment settings"));
        // 0600 on unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&env).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "secrets file must be owner-only");
        }
    }

    #[test]
    fn env_writer_creates_fresh_file() {
        let dir = tempfile::tempdir().unwrap();
        let env = dir.path().join(".env");
        assert!(!env.exists());
        let written = write_env_file_at(&env, &[("OWNER_ID".to_string(), "42".to_string())])
            .expect("fresh file must be created");
        assert_eq!(written, env);
        assert_eq!(std::fs::read_to_string(&env).unwrap(), "OWNER_ID=42\n");
        // An empty updates list still yields a valid (headerless) file.
        let empty = dir.path().join("empty.env");
        write_env_file_at(&empty, &[]).unwrap();
        assert_eq!(std::fs::read_to_string(&empty).unwrap(), "");
    }

    #[test]
    fn env_writer_fails_closed_on_unreadable_existing_file() {
        // A directory where a file is expected: read_to_string always fails
        // (EISDIR), for every user including root — a deterministic way to
        // exercise the fail-closed read path.
        let dir = tempfile::tempdir().unwrap();
        let env = dir.path().join(".env");
        std::fs::create_dir(&env).unwrap();
        let res = write_env_file_at(&env, &[("OWNER_ID".to_string(), "1".to_string())]);
        assert!(res.is_err(), "unreadable existing .env must fail closed");
        assert!(env.is_dir(), "existing content must not be clobbered");
    }

    #[test]
    fn env_path_resolution_order() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("cwd");
        let data = dir.path().join("data");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let cwd_env = cwd.join(".env");
        let data_env = data.join(".env");
        // An explicit ENV_FILE wins even when the file does not exist yet.
        let explicit = dir.path().join("explicit.env");
        assert_eq!(
            resolve_env_path(Some(explicit.to_str().unwrap()), &cwd_env, &data_env),
            explicit
        );
        // A blank ENV_FILE value falls through to the normal order.
        assert_eq!(resolve_env_path(Some("  "), &cwd_env, &data_env), cwd_env);
        // Neither exists: a fresh file lands in the cwd.
        assert_eq!(resolve_env_path(None, &cwd_env, &data_env), cwd_env);
        // Only the data-dir copy exists: it is picked up.
        std::fs::write(&data_env, "X=1\n").unwrap();
        assert_eq!(resolve_env_path(None, &cwd_env, &data_env), data_env);
        // Both exist: cwd wins.
        std::fs::write(&cwd_env, "Y=1\n").unwrap();
        assert_eq!(resolve_env_path(None, &cwd_env, &data_env), cwd_env);
    }
}
