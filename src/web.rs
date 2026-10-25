use axum::{
    Router,
    extract::{Form, Path, State},
    http::header,
    response::{Html, IntoResponse, Redirect},
    routing::{get, post},
};
use serde::Deserialize;
use std::sync::Arc;
use teloxide::{Bot, prelude::*};
use tower_http::services::ServeDir;

use crate::{config::Config, db::Db};

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct WebState {
    pub db: Arc<Db>,
    pub cfg: Config,
    pub bot: Bot,
    pub login_attempts: Arc<StdMutex<HashMap<String, (u32, Instant)>>>,
    /// Base path the dashboard is served under (`""` = root). All generated
    /// URLs, redirects and cookie paths are prefixed with it.
    pub base_path: String,
}

impl WebState {
    /// An in-app absolute path prefixed with the configured base path.
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_path, path)
    }

    /// `Path` attribute for session cookies: the base path when mounted under
    /// one, `/` when serving at the root.
    pub fn cookie_path(&self) -> String {
        if self.base_path.is_empty() {
            "/".to_string()
        } else {
            self.base_path.clone()
        }
    }
}

/// Records one failed login attempt for `key` and returns whether the key is
/// currently locked out (`failures >= max` within `window_secs`, measured from
/// the first failure of the current window). The counter resets on window
/// expiry; a successful login should call [`login_ok`] to clear it. When the
/// map grows past 1024 entries, entries older than one hour are evicted so it
/// stays bounded.
pub fn login_throttle(
    map: &std::sync::Mutex<std::collections::HashMap<String, (u32, std::time::Instant)>>,
    key: &str,
    max: u32,
    window_secs: u64,
) -> bool {
    let mut map = map.lock().unwrap();
    let now = Instant::now();
    if map.len() > 1024 {
        map.retain(|_, at| now.duration_since(at.1) < Duration::from_secs(3600));
    }
    let entry = map.entry(key.to_string()).or_insert((0, now));
    if now.duration_since(entry.1) >= Duration::from_secs(window_secs) {
        *entry = (0, now);
    }
    entry.0 = entry.0.saturating_add(1);
    entry.0 >= max
}

/// Clears the failure counter for `key` after a successful login.
pub fn login_ok(
    map: &std::sync::Mutex<std::collections::HashMap<String, (u32, std::time::Instant)>>,
    key: &str,
) {
    map.lock().unwrap().remove(key);
}

/// HTML-escapes a dynamic value before it is interpolated into an HTML body
/// or an attribute value. Must be applied to every non-numeric dynamic value.
/// Public so the fuzz targets can exercise it.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Whether session cookies should carry the `Secure` attribute.
fn secure_cookies() -> bool {
    std::env::var("WEB_URL")
        .map(|u| u.starts_with("https"))
        .unwrap_or(false)
        || std::env::var("WEB_SECURE")
            .map(|v| v == "1")
            .unwrap_or(false)
}

/// Authoritative first-run gate: the `setup_done` DB flag written by
/// `/setup` wins over the startup-frozen env snapshot, so a completed setup
/// cannot be replayed before a restart.
fn setup_done(state: &WebState) -> bool {
    state.db.get_config("setup_done").is_some() || state.cfg.is_configured()
}

/// `.env` keys owned by the setup form: each submit rewrites exactly these.
/// Every other line in an existing `.env` (WEB_BIND, WEB_URL, WEB_BASE_PATH,
/// OPEN_BROWSER, ...) is preserved verbatim so a re-run never drops
/// deployment settings it knows nothing about.
const SETUP_ENV_KEYS: [&str; 9] = [
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

pub fn router(state: WebState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/setup", get(setup_page).post(setup_submit))
        .route("/login", get(login_page).post(login_submit))
        .route("/logout", get(logout))
        .route("/dashboard", get(dashboard))
        .route("/channels", get(channels_page))
        .route("/plans", get(plans_page))
        .route("/posts", get(posts_page))
        .route("/post", post(create_post))
        .route("/plan", post(create_plan))
        .route("/plandel", post(delete_plan_by_name))
        .route("/plans/{id}/delete", post(delete_plan_by_id))
        .route("/products", get(products_page))
        .route("/product", post(create_product))
        .route("/productdel", post(delete_product))
        .nest_service("/static", ServeDir::new("static"))
        .with_state(state)
}

/// Mount `router` under a base path. An empty base returns the router
/// unchanged — `Router::nest("")` panics, so the guard is load-bearing.
/// `base + "/"` (trailing slash) is redirected to `base`: axum's nest maps
/// it to an empty inner path, which matches no route.
pub fn mount(base: &str, router: Router) -> Router {
    if base.is_empty() {
        return router;
    }
    let target = base.to_string();
    let trailing = format!("{base}/");
    Router::new()
        .nest(base, router)
        .route(&trailing, get(move || async move { Redirect::to(&target) }))
}

fn html_page(base: &str, title: &str, body: &str) -> String {
    format!(
        r#"<!DOCTYPE html><html lang="en" data-theme="light"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{t} — Telegram Business</title>
<link rel="preconnect" href="https://fonts.googleapis.com"><link rel="preconnect" href="https://fonts.gstatic.com" crossorigin><link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Fira+Code:wght@400;500;600&family=Fira+Sans:wght@300;400;500;600&display=swap">
<script>(function(){{var s=localStorage.getItem('theme');var d=window.matchMedia('(prefers-color-scheme: dark)').matches;document.documentElement.setAttribute('data-theme',s||(d?'dark':'light'));}})();</script>
<style>
:root {{
  --color-primary:#0F172A; --color-on-primary:#FFFFFF;
  --color-secondary:#1E293B; --color-on-secondary:#FFFFFF;
  --color-accent:#16A34A; --color-on-accent:#0F172A;
  --color-background:#F8FAFC; --color-foreground:#0F172A;
  --color-card:#FFFFFF; --color-card-foreground:#0F172A;
  --color-muted:#E2E8F0; --color-muted-foreground:#475569;
  --color-border:#CBD5E1; --color-destructive:#DC2626; --color-on-destructive:#FFFFFF;
  --color-ring:#16A34A;
  --color-success:#16A34A; --color-warning:#D97706;
  --font-heading:"Fira Code","SF Mono",ui-monospace,monospace;
  --font-body:"Fira Sans",-apple-system,BlinkMacSystemFont,"SF Pro Text","Helvetica Neue",sans-serif;
  --space-xs:4px; --space-sm:8px; --space-md:16px; --space-lg:24px; --space-xl:32px; --space-2xl:48px; --space-3xl:64px;
  --radius-sm:8px; --radius-md:12px; --radius-lg:16px;
  --shadow-sm:0 1px 2px rgba(0,0,0,0.05);
  --shadow-md:0 4px 6px rgba(0,0,0,0.1);
  --shadow-lg:0 10px 15px rgba(0,0,0,0.1);
  --shadow-xl:0 20px 25px rgba(0,0,0,0.15);
  --transition:200ms ease;
}}
[data-theme="dark"] {{
  --color-primary:#0F172A; --color-on-primary:#FFFFFF;
  --color-secondary:#1E293B; --color-on-secondary:#FFFFFF;
  --color-accent:#16A34A; --color-on-accent:#0F172A;
  --color-background:#020617; --color-foreground:#F8FAFC;
  --color-card:#0E1223; --color-card-foreground:#F8FAFC;
  --color-muted:#1A1E2F; --color-muted-foreground:#94A3B8;
  --color-border:#334155; --color-destructive:#DC2626; --color-on-destructive:#FFFFFF;
  --color-ring:#FFFFFF;
  --color-success:#16A34A; --color-warning:#F59E0B;
  --shadow-sm:0 1px 2px rgba(0,0,0,0.4);
  --shadow-md:0 4px 6px rgba(0,0,0,0.5);
  --shadow-lg:0 10px 15px rgba(0,0,0,0.6);
  --shadow-xl:0 20px 25px rgba(0,0,0,0.7);
}}
@media (prefers-color-scheme: dark) {{
  :root:not([data-theme="light"]) {{
    --color-primary:#0F172A; --color-on-primary:#FFFFFF;
    --color-secondary:#1E293B; --color-on-secondary:#FFFFFF;
    --color-accent:#16A34A; --color-on-accent:#0F172A;
    --color-background:#020617; --color-foreground:#F8FAFC;
    --color-card:#0E1223; --color-card-foreground:#F8FAFC;
    --color-muted:#1A1E2F; --color-muted-foreground:#94A3B8;
    --color-border:#334155; --color-destructive:#DC2626; --color-on-destructive:#FFFFFF;
    --color-ring:#FFFFFF; --color-success:#16A34A; --color-warning:#F59E0B;
  }}
}}
*{{box-sizing:border-box;margin:0}}
html{{scroll-behavior:smooth}}
body{{margin:0;font-family:var(--font-body);background:var(--color-background);color:var(--color-foreground);font-size:15px;line-height:1.5;-webkit-font-smoothing:antialiased}}
a{{color:var(--color-accent);text-decoration:none;transition:opacity var(--transition)}}
a:hover{{opacity:0.8}}
.container{{max-width:980px;margin:0 auto;padding:var(--space-xl) var(--space-md)}}
.nav{{display:flex;flex-wrap:wrap;gap:var(--space-md);align-items:center;margin-bottom:var(--space-xl);font-size:14px;padding-bottom:var(--space-md);border-bottom:1px solid var(--color-border)}}
.nav a{{color:var(--color-muted-foreground);font-weight:500;transition:color var(--transition)}}
.nav a:hover{{color:var(--color-foreground)}}
.nav .spacer{{flex:1}}
.nav button.theme-toggle{{margin-left:auto}}
h1{{font-family:var(--font-heading);font-size:28px;font-weight:600;letter-spacing:-0.02em;margin:0 0 var(--space-lg)}}
h2{{font-family:var(--font-heading);font-size:22px;font-weight:600;letter-spacing:-0.01em;margin:0 0 var(--space-md)}}
h3{{font-family:var(--font-heading);font-size:18px;font-weight:600;margin:0 0 var(--space-sm)}}
p{{margin:0 0 var(--space-sm)}}
.card{{background:var(--color-card);color:var(--color-card-foreground);border:1px solid var(--color-border);border-radius:var(--radius-md);box-shadow:var(--shadow-md);padding:var(--space-lg);margin-bottom:var(--space-md);transition:box-shadow var(--transition),transform var(--transition)}}
.card:hover{{box-shadow:var(--shadow-lg);transform:translateY(-2px)}}
.card p{{margin:8px 0;color:var(--color-muted-foreground)}}
button{{display:inline-flex;align-items:center;justify-content:center;gap:var(--space-sm);background:var(--color-accent);color:var(--color-on-accent);padding:10px 20px;border-radius:var(--radius-sm);font-size:14px;font-weight:600;border:1px solid transparent;cursor:pointer;transition:all var(--transition);font-family:inherit}}
button:hover{{opacity:0.9;transform:translateY(-1px)}}
button:active{{transform:translateY(0)}}
button:focus-visible{{outline:2px solid var(--color-ring);outline-offset:2px}}
.btn{{display:inline-flex;align-items:center;justify-content:center;gap:var(--space-sm);background:var(--color-accent);color:var(--color-on-accent);padding:10px 20px;border-radius:var(--radius-sm);font-size:14px;font-weight:600;border:1px solid transparent;cursor:pointer;transition:all var(--transition);font-family:inherit}}
.btn:hover{{opacity:0.9;transform:translateY(-1px)}}
.btn:active{{transform:translateY(0)}}
.btn:focus-visible{{outline:2px solid var(--color-ring);outline-offset:2px}}
.btn-secondary{{background:transparent;color:var(--color-accent);border-color:var(--color-accent)}}
.btn-secondary:hover{{background:var(--color-accent);color:var(--color-on-accent)}}
.btn-danger{{background:var(--color-destructive);color:var(--color-on-destructive)}}
.btn-ghost{{background:transparent;color:var(--color-muted-foreground);border-color:transparent;padding:8px 12px}}
.btn-ghost:hover{{background:var(--color-muted);color:var(--color-foreground)}}
.btn-sm{{padding:6px 12px;font-size:13px}}
.input,textarea,select{{width:100%;padding:10px 12px;border:1px solid var(--color-border);border-radius:var(--radius-sm);font-size:15px;font-family:inherit;background:var(--color-card);color:var(--color-foreground);margin:6px 0 12px;transition:border-color var(--transition),box-shadow var(--transition)}}
.input:focus,textarea:focus,select:focus{{outline:none;border-color:var(--color-accent);box-shadow:0 0 0 3px color-mix(in srgb,var(--color-accent) 20%,transparent)}}
textarea{{min-height:96px;resize:vertical}}
label{{font-size:13px;color:var(--color-muted-foreground);display:block;margin-top:8px;font-weight:500}}
.form-group{{margin-bottom:var(--space-md)}}
.form-row{{display:grid;gap:var(--space-md)}}
ul{{list-style:none;padding:0;margin:0}}
li{{padding:10px 0;border-bottom:1px solid var(--color-border);font-size:14px}}
li:last-child{{border-bottom:none}}
form{{margin:0}}
article{{background:var(--color-card);border:1px solid var(--color-border);border-radius:var(--radius-md);box-shadow:var(--shadow-md);padding:var(--space-lg)}}
.empty{{text-align:center;color:var(--color-muted-foreground);padding:var(--space-2xl) 0}}
.table-wrap{{overflow-x:auto;border:1px solid var(--color-border);border-radius:var(--radius-md)}}
table{{width:100%;border-collapse:collapse;font-size:14px}}
th,td{{padding:12px 16px;text-align:left;border-bottom:1px solid var(--color-border)}}
th{{font-weight:600;color:var(--color-muted-foreground);background:var(--color-muted);font-size:12px;text-transform:uppercase;letter-spacing:0.05em}}
tr:last-child td{{border-bottom:none}}
tr:hover td{{background:var(--color-muted)}}
.badge{{display:inline-block;padding:2px 8px;border-radius:999px;font-size:12px;font-weight:500;background:var(--color-muted);color:var(--color-muted-foreground)}}
.badge-success{{background:color-mix(in srgb,var(--color-success) 15%,transparent);color:var(--color-success)}}
.badge-danger{{background:color-mix(in srgb,var(--color-destructive) 15%,transparent);color:var(--color-destructive)}}
.badge-warning{{background:color-mix(in srgb,var(--color-warning) 15%,transparent);color:var(--color-warning)}}
.modal-overlay{{position:fixed;inset:0;background:rgba(0,0,0,0.5);backdrop-filter:blur(4px);display:flex;align-items:center;justify-content:center;z-index:50}}
.modal{{background:var(--color-card);border-radius:var(--radius-lg);padding:var(--space-xl);box-shadow:var(--shadow-xl);max-width:500px;width:90%}}
.sr-only{{position:absolute;width:1px;height:1px;padding:0;margin:-1px;overflow:hidden;clip:rect(0,0,0,0);white-space:nowrap;border:0}}
@media (max-width:768px){{.container{{padding:var(--space-lg) var(--space-sm)}}.nav{{gap:var(--space-sm)}}h1{{font-size:24px}}.card{{padding:var(--space-md)}}}}
@media (max-width:480px){{.container{{padding:var(--space-md) var(--space-sm)}}h1{{font-size:22px}}}}
@media (prefers-reduced-motion:reduce){{*{{transition:none!important;animation:none!important}}}}
</style></head><body><main class="container">
<nav class="nav"><a href="{base}/dashboard">Dashboard</a><a href="{base}/channels">Channels</a><a href="{base}/plans">Plans</a><a href="{base}/products">Products</a><a href="{base}/posts">Posts</a><a href="{base}/logout">Logout</a><button type="button" class="btn btn-ghost btn-sm theme-toggle" onclick="toggleTheme()">Theme</button></nav>
{b}</main>
<script>function toggleTheme(){{var c=document.documentElement.getAttribute('data-theme');var n=c==='dark'?'light':'dark';document.documentElement.setAttribute('data-theme',n);localStorage.setItem('theme',n);}}</script>
</body></html>"#,
        base = base,
        t = title,
        b = body
    )
}

#[derive(Clone)]
pub struct Session {
    pub account_id: i64,
    pub role: String,
    pub seller_id: Option<i64>,
}

pub fn get_cookie(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.split(';').find_map(|p| {
                let p = p.trim();
                p.strip_prefix("session=").map(|v| v.to_string())
            })
        })
}

pub fn session_from_headers(state: &WebState, headers: &axum::http::HeaderMap) -> Option<Session> {
    // Cookie
    if let Some(token) = get_cookie(headers)
        && let Some(s) = try_session(state, &token)
    {
        return Some(s);
    }
    // Bearer
    if let Some(auth) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        && let Some(tok) = auth.strip_prefix("Bearer ")
        && let Some(s) = try_session(state, tok)
    {
        return Some(s);
    }
    None
}

fn try_session(state: &WebState, token: &str) -> Option<Session> {
    let (account_id, role, seller_id) = state.db.get_web_session(token)?;
    Some(Session {
        account_id,
        role,
        seller_id,
    })
}

async fn index(State(state): State<WebState>, headers: axum::http::HeaderMap) -> impl IntoResponse {
    if !setup_done(&state) {
        return Redirect::to(&state.url("/setup")).into_response();
    }
    if session_from_headers(&state, &headers).is_some() {
        Redirect::to(&state.url("/dashboard")).into_response()
    } else {
        Redirect::to(&state.url("/login")).into_response()
    }
}

async fn login_page(State(state): State<WebState>) -> impl IntoResponse {
    if !setup_done(&state) {
        return Redirect::to(&state.url("/setup")).into_response();
    }
    Html(html_page(
        &state.base_path,
        "Login",
        &format!(
            r#"<article><h2>Login</h2><form method="post" action="{}"><label>Username<input type="text" name="username" required></label><label>Password<input type="password" name="password" required></label><button type="submit">Sign in</button></form></article>"#,
            state.url("/login")
        ),
    ))
    .into_response()
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

async fn login_submit(
    State(state): State<WebState>,
    Form(f): Form<LoginForm>,
) -> impl IntoResponse {
    // Rate limit: 5 failed attempts per username per 15 minutes
    // (a successful login clears the counter via `login_ok`).
    if login_throttle(&state.login_attempts, &f.username, 5, 900) {
        return (
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            Html(html_page(
                &state.base_path,
                "Login",
                "<h2>Login</h2><p style=\"color:red\">Too many attempts. Try again later.</p>",
            )),
        )
            .into_response();
    }
    if let Some((account_id, role, seller_id)) =
        state.db.verify_web_password(&f.username, &f.password)
    {
        login_ok(&state.login_attempts, &f.username);
        let token = state.db.create_web_session(account_id, &role, seller_id, 7);
        let cookie = if secure_cookies() {
            format!(
                "session={}; Path={}; HttpOnly; SameSite=Strict; Secure; Max-Age=604800",
                token,
                state.cookie_path()
            )
        } else {
            format!(
                "session={}; Path={}; HttpOnly; SameSite=Strict; Max-Age=604800",
                token,
                state.cookie_path()
            )
        };
        (
            [(header::SET_COOKIE, cookie)],
            Redirect::to(&state.url("/dashboard")),
        )
            .into_response()
    } else {
        // The failure was already recorded by `login_throttle` above.
        Html(html_page(
            &state.base_path,
            "Login",
            &format!(
                r#"<article><h2>Login</h2><p style="color:red">Invalid credentials.</p><form method="post" action="{}"><label>Username<input type="text" name="username" required></label><label>Password<input type="password" name="password" required></label><button type="submit">Sign in</button></form></article>"#,
                state.url("/login")
            ),
        ))
        .into_response()
    }
}

async fn logout(
    State(state): State<WebState>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if let Some(token) = get_cookie(&headers) {
        state.db.delete_web_session(&token);
    }
    let cookie = if secure_cookies() {
        format!(
            "session=; Path={}; HttpOnly; SameSite=Strict; Secure; Max-Age=0",
            state.cookie_path()
        )
    } else {
        format!(
            "session=; Path={}; HttpOnly; SameSite=Strict; Max-Age=0",
            state.cookie_path()
        )
    };
    (
        [(header::SET_COOKIE, cookie)],
        Redirect::to(&state.url("/login")),
    )
        .into_response()
}

async fn dashboard(
    State(state): State<WebState>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let session = match session_from_headers(&state, &headers) {
        Some(s) => s,
        None => return Redirect::to(&state.url("/login")).into_response(),
    };
    let seller_scope = if session.role == "seller" {
        session.seller_id
    } else {
        None
    };
    let ch = state
        .db
        .get_config("channel")
        .unwrap_or_else(|| "(none)".into());
    let plans = state.db.list_plans(seller_scope);
    let subs = state.db.list_subscriptions(seller_scope, 10);
    let body = format!(
        r#"<h1>Dashboard ({})</h1>
<article class="card"><h3>Default channel</h3><p>{}</p></article>
<article class="card"><h3>Plans ({})</h3><ul>{}</ul></article>
<article class="card"><h3>Create plan</h3><form method="post" action="{}">
<label>Name<input name="name" required></label>
<label>Price<input name="price" required></label>
<label>Asset<input name="asset" required></label>
<label>Duration days<input name="duration_days" type="number" required></label>
<label>Channel<input name="channel" placeholder="@channel (optional)"></label>
<button type="submit">Create plan</button></form></article>
<article class="card"><h3>Recent subscriptions ({})</h3><ul>{}</ul></article>
<article class="card"><h3>Post</h3><form method="post" action="{}">
<label>Channel<input name="channel" placeholder="@channel (optional)"></label>
<label>Text<textarea name="text" required></textarea></label>
<button type="submit">Post</button></form></article>"#,
        esc(&session.role),
        esc(&ch),
        plans.len(),
        plans
            .iter()
            .map(|p| {
                format!(
                    "<li>{} — {} {} / {} days</li>",
                    esc(&p.0),
                    esc(&p.1),
                    esc(&p.2),
                    p.3
                )
            })
            .collect::<Vec<_>>()
            .join(""),
        state.url("/plan"),
        subs.len(),
        subs.iter()
            .map(|s| {
                format!(
                    "<li>{} user {} {} expires {}</li>",
                    s.0,
                    s.1,
                    esc(&s.3),
                    esc(&s.4)
                )
            })
            .collect::<Vec<_>>()
            .join(""),
        state.url("/post"),
    );
    Html(html_page(&state.base_path, "Dashboard", &body)).into_response()
}

async fn channels_page(
    State(state): State<WebState>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let session = match session_from_headers(&state, &headers) {
        Some(s) => s,
        None => return Redirect::to(&state.url("/login")).into_response(),
    };
    let seller_scope = if session.role == "seller" {
        session.seller_id
    } else {
        None
    };
    let mut set = std::collections::BTreeSet::new();
    if let Some(c) = state.db.get_config("channel") {
        set.insert(c);
    }
    if let Some(c) = &state.cfg.default_channel {
        set.insert(c.clone());
    }
    for p in state.db.list_plans(seller_scope) {
        if let Some(c) = p.4 {
            set.insert(c);
        }
    }
    let body = format!(
        "<h1>Channels</h1><ul>{}</ul>",
        set.iter()
            .map(|c| format!("<li>{}</li>", esc(c)))
            .collect::<Vec<_>>()
            .join("")
    );
    Html(html_page(&state.base_path, "Channels", &body)).into_response()
}

async fn plans_page(
    State(state): State<WebState>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let session = match session_from_headers(&state, &headers) {
        Some(s) => s,
        None => return Redirect::to(&state.url("/login")).into_response(),
    };
    let seller_scope = if session.role == "seller" {
        session.seller_id
    } else {
        None
    };
    let plans = state.db.list_plans(seller_scope);
    let list_items = plans
        .iter()
        .map(|p| {
            format!(
                "<li>{} — {} {} / {} days <form method=\"post\" action=\"{}\" style=\"display:inline\"><input type=\"hidden\" name=\"name\" value=\"{}\"><button type=\"submit\">Delete</button></form></li>",
                esc(&p.0),
                esc(&p.1),
                esc(&p.2),
                p.3,
                state.url("/plandel"),
                esc(&p.0)
            )
        })
        .collect::<Vec<_>>()
        .join("");
    let body = format!(
        r#"<h1>Plans</h1>
<ul>{}</ul>
<article class="card"><h3>Create plan</h3><form method="post" action="{}">
<label>Name<input name="name" required></label>
<label>Price<input name="price" required></label>
<label>Asset<input name="asset" required></label>
<label>Duration days<input name="duration_days" type="number" required></label>
<label>Channel<input name="channel" placeholder="@channel (optional)"></label>
<button type="submit">Create plan</button></form></article>"#,
        list_items,
        state.url("/plan")
    );
    Html(html_page(&state.base_path, "Plans", &body)).into_response()
}

async fn posts_page(
    State(state): State<WebState>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let session = match session_from_headers(&state, &headers) {
        Some(s) => s,
        None => return Redirect::to(&state.url("/login")).into_response(),
    };
    let _ = session;
    Html(html_page(
        &state.base_path,
        "Posts",
        "<h1>Posts</h1><p>Use Telegram commands or the dashboard form to post.</p>",
    ))
    .into_response()
}

#[derive(Deserialize)]
struct PostForm {
    channel: Option<String>,
    text: String,
}

async fn create_post(
    State(state): State<WebState>,
    headers: axum::http::HeaderMap,
    Form(f): Form<PostForm>,
) -> impl IntoResponse {
    let session = match session_from_headers(&state, &headers) {
        Some(s) => s,
        None => return Redirect::to(&state.url("/login")).into_response(),
    };
    let ch = f
        .channel
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| state.db.get_config("channel").unwrap_or_default());
    if ch.is_empty() || f.text.trim().is_empty() {
        return Redirect::to(&state.url("/dashboard")).into_response();
    }
    if let Err(e) = state
        .bot
        .send_message(crate::util::to_chat_id(&ch), f.text.clone())
        .await
    {
        tracing::warn!("failed to send post: {}", e);
    }
    let seller_id = session.seller_id.unwrap_or(0);
    if let Err(e) = state.db.log_post(seller_id, &ch, 0, "text", Some(&f.text)) {
        tracing::warn!("failed to log post: {}", e);
    }
    Redirect::to(&state.url("/dashboard")).into_response()
}

#[derive(Deserialize)]
struct PlanForm {
    name: String,
    price: String,
    asset: String,
    duration_days: String,
    channel: Option<String>,
}

async fn create_plan(
    State(state): State<WebState>,
    headers: axum::http::HeaderMap,
    Form(f): Form<PlanForm>,
) -> impl IntoResponse {
    let session = match session_from_headers(&state, &headers) {
        Some(s) => s,
        None => return Redirect::to(&state.url("/login")).into_response(),
    };
    let seller_id = session.seller_id.unwrap_or(0);
    let name = f.name.trim().to_string();
    let price = f.price.trim().to_string();
    let asset = f.asset.trim().to_string();
    let days: i64 = match f.duration_days.trim().parse() {
        Ok(v) if v > 0 => v,
        _ => return Redirect::to(&state.url("/plans")).into_response(),
    };
    if name.is_empty() || price.is_empty() || asset.is_empty() {
        return Redirect::to(&state.url("/plans")).into_response();
    }
    let channel = f.channel.filter(|s| !s.trim().is_empty());
    state
        .db
        .add_plan(seller_id, &name, &price, &asset, days, channel.as_deref());
    Redirect::to(&state.url("/plans")).into_response()
}

#[derive(Deserialize)]
struct PlanDeleteForm {
    name: Option<String>,
    plan: Option<String>,
    plan_name: Option<String>,
}

async fn delete_plan_by_name(
    State(state): State<WebState>,
    headers: axum::http::HeaderMap,
    Form(f): Form<PlanDeleteForm>,
) -> impl IntoResponse {
    let session = match session_from_headers(&state, &headers) {
        Some(s) => s,
        None => return Redirect::to(&state.url("/login")).into_response(),
    };
    let name = f
        .name
        .or(f.plan)
        .or(f.plan_name)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let Some(name) = name else {
        return Redirect::to(&state.url("/plans")).into_response();
    };
    if let Some(sid) = session.seller_id {
        state.db.remove_plan(sid, &name);
    } else if let Some(global) = state.db.get_plan_global(&name) {
        state.db.remove_plan(global.0, &name);
    } else {
        state.db.remove_plan(0, &name);
    }
    Redirect::to(&state.url("/plans")).into_response()
}

async fn delete_plan_by_id(
    State(state): State<WebState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let session = match session_from_headers(&state, &headers) {
        Some(s) => s,
        None => return Redirect::to(&state.url("/login")).into_response(),
    };
    if let Some((sid, price, asset, name, days, channel)) = state.db.get_plan_by_id(id) {
        if session.role == "seller" && session.seller_id != Some(sid) {
            return Redirect::to(&state.url("/plans")).into_response();
        }
        let _ = (price, asset, days, channel);
        state.db.remove_plan(sid, &name);
    }
    Redirect::to(&state.url("/plans")).into_response()
}

async fn products_page(
    State(state): State<WebState>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let session = match session_from_headers(&state, &headers) {
        Some(s) => s,
        None => return Redirect::to(&state.url("/login")).into_response(),
    };
    let seller_scope = if session.role == "seller" {
        session.seller_id
    } else {
        None
    };
    let products = state.db.list_products(seller_scope);
    let listing = if products.is_empty() {
        "<p class=\"empty\">No products yet.</p>".to_string()
    } else {
        let rows = products
            .iter()
            .map(|p| {
                // Product tuple: (id, seller_id, name, price, asset, kind,
                // delivery, stock, active).
                let stock = match p.7 {
                    Some(n) => n.to_string(),
                    None => "—".to_string(),
                };
                let active = if p.8 != 0 {
                    "<span class=\"badge badge-success\">active</span>"
                } else {
                    "<span class=\"badge badge-danger\">inactive</span>"
                };
                format!(
                    "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td><form method=\"post\" action=\"{}\" style=\"display:inline\"><input type=\"hidden\" name=\"name\" value=\"{}\"><button type=\"submit\" class=\"btn btn-danger btn-sm\">Deactivate</button></form></td></tr>",
                    esc(&p.2),
                    esc(&p.3),
                    esc(&p.4),
                    esc(&p.5),
                    stock,
                    active,
                    state.url("/productdel"),
                    esc(&p.2)
                )
            })
            .collect::<Vec<_>>()
            .join("");
        format!(
            "<div class=\"table-wrap\"><table><thead><tr><th>Name</th><th>Price</th><th>Asset</th><th>Kind</th><th>Stock</th><th>Active</th><th></th></tr></thead><tbody>{}</tbody></table></div>",
            rows
        )
    };
    let body = format!(
        r#"<h1>Products</h1>
{}
<article class="card"><h3>Create product</h3><form method="post" action="{}">
<label>Name<input name="name" required></label>
<label>Price<input name="price" required></label>
<label>Asset<input name="asset" required></label>
<label>Kind<select name="kind"><option value="digital_link">digital_link</option><option value="digital_file">digital_file</option><option value="physical">physical</option></select></label>
<label>Delivery (link, text or Telegram file_id, optional)<input name="delivery"></label>
<label>Stock (blank = unlimited)<input name="stock" type="number" min="0"></label>
<button type="submit">Create product</button></form></article>"#,
        listing,
        state.url("/product")
    );
    Html(html_page(&state.base_path, "Products", &body)).into_response()
}

#[derive(Deserialize)]
struct ProductForm {
    name: String,
    price: String,
    asset: String,
    kind: String,
    delivery: Option<String>,
    stock: Option<String>,
}

async fn create_product(
    State(state): State<WebState>,
    headers: axum::http::HeaderMap,
    Form(f): Form<ProductForm>,
) -> impl IntoResponse {
    let session = match session_from_headers(&state, &headers) {
        Some(s) => s,
        None => return Redirect::to(&state.url("/login")).into_response(),
    };
    let seller_id = session.seller_id.unwrap_or(0);
    let name = f.name.trim().to_string();
    let price = f.price.trim().to_string();
    let asset = f.asset.trim().to_string();
    let kind = f.kind.trim().to_string();
    if name.is_empty() || price.is_empty() || asset.is_empty() {
        return Redirect::to(&state.url("/products")).into_response();
    }
    if !matches!(kind.as_str(), "digital_link" | "digital_file" | "physical") {
        return Redirect::to(&state.url("/products")).into_response();
    }
    let delivery = f
        .delivery
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let stock: Option<i64> = match f.stock.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => match s.parse() {
            Ok(v) if v >= 0 => Some(v),
            _ => return Redirect::to(&state.url("/products")).into_response(),
        },
        None => None,
    };
    if let Err(e) = state.db.add_product(
        seller_id,
        &name,
        &price,
        &asset,
        &kind,
        delivery.as_deref(),
        stock,
    ) {
        return Html(html_page(
            &state.base_path,
            "Products",
            &format!(
                r#"<h1>Products</h1><p style="color:red">{}</p><p><a href="{}">Back to products</a></p>"#,
                esc(&e.to_string()),
                state.url("/products")
            ),
        ))
        .into_response();
    }
    Redirect::to(&state.url("/products")).into_response()
}

#[derive(Deserialize)]
struct ProductDeleteForm {
    name: String,
}

async fn delete_product(
    State(state): State<WebState>,
    headers: axum::http::HeaderMap,
    Form(f): Form<ProductDeleteForm>,
) -> impl IntoResponse {
    let session = match session_from_headers(&state, &headers) {
        Some(s) => s,
        None => return Redirect::to(&state.url("/login")).into_response(),
    };
    let seller = session.seller_id.unwrap_or(0);
    let name = f.name.trim().to_string();
    if !name.is_empty() {
        state.db.set_product_active(seller, &name, false);
    }
    Redirect::to(&state.url("/products")).into_response()
}

#[derive(Deserialize)]
struct SetupForm {
    bot_token: String,
    owner_id: String,
    default_channel: Option<String>,
    crypto_token: Option<String>,
    web_username: Option<String>,
    web_password: Option<String>,
}

async fn setup_page(State(state): State<WebState>) -> impl IntoResponse {
    if setup_done(&state) {
        return Redirect::to(&state.url("/login")).into_response();
    }
    let body = format!(
        r#"<article><h2>Welcome — first-run setup</h2>
<p>Configure your bot. You can also edit <code>.env</code> later.</p>
<details open><summary>How to get a bot token (2 minutes, no skills needed)</summary>
<ol><li>Open Telegram and message <a href="https://t.me/BotFather" rel="noopener">@BotFather</a>.</li>
<li>Send <code>/newbot</code>, choose a display name, then a username ending in <code>bot</code>.</li>
<li>Copy the token it replies with (<code>123456:ABC-…</code>) and paste it below.</li>
<li>For your owner ID, message <a href="https://t.me/userinfobot" rel="noopener">@userinfobot</a> — it replies with your numeric ID.</li>
<li>Keep the bot's privacy mode ON; add it as <b>admin</b> to your channel when you sell access.</li></ol></details>
<form method="post" action="{}">
<label>Bot token (from @BotFather)<input type="text" name="bot_token" required></label>
<label>Owner Telegram user ID<input type="text" name="owner_id" required></label>
<label>Default channel (optional, e.g. @yourchannel)<input type="text" name="default_channel"></label>
<label>Crypto Pay token (optional)<input type="text" name="crypto_token"></label>
<label>Dashboard username<input type="text" name="web_username" value="owner"></label>
<label>Dashboard password (min 6 chars)<input type="password" name="web_password" required></label>
<button type="submit">Save and continue</button>
</form></article>"#,
        state.url("/setup")
    );
    Html(html_page(&state.base_path, "Setup", &body)).into_response()
}

async fn setup_submit(
    State(state): State<WebState>,
    Form(f): Form<SetupForm>,
) -> impl IntoResponse {
    if setup_done(&state) {
        return Redirect::to(&state.url("/login")).into_response();
    }
    // Validate bot token.
    let token = f.bot_token.trim().to_string();
    if token.is_empty() {
        return Html(html_page(
            &state.base_path,
            "Setup",
            "<p style=\"color:red\">Bot token is required.</p>",
        ))
        .into_response();
    }
    let ok = validate_bot_token(&token).await;
    if !ok {
        return Html(html_page(
            &state.base_path,
            "Setup",
            "<p style=\"color:red\">Invalid bot token (Telegram rejected it).</p>",
        ))
        .into_response();
    }
    let owner_id: i64 = match f.owner_id.trim().parse() {
        Ok(v) if v > 0 => v,
        _ => {
            return Html(html_page(
                &state.base_path,
                "Setup",
                "<p style=\"color:red\">Owner ID must be a positive number.</p>",
            ))
            .into_response();
        }
    };
    let uname = f
        .web_username
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "owner".to_string());
    let pw = f.web_password.clone().unwrap_or_default();
    if pw.len() < 6 {
        return Html(html_page(
            &state.base_path,
            "Setup",
            "<p style=\"color:red\">Password must be at least 6 characters.</p>",
        ))
        .into_response();
    }
    let channel = f.default_channel.clone().filter(|s| !s.trim().is_empty());
    let crypto = f.crypto_token.clone().filter(|s| !s.trim().is_empty());

    // Write .env (secrets stay in env file, not DB). The file to (re)write
    // mirrors `config::load_dotenv`'s lookup: an explicit ENV_FILE wins,
    // then the cwd `.env`, else the data-dir copy. An existing file is read
    // first and every line whose key the form does not own is preserved.
    let env_path = match std::env::var("ENV_FILE") {
        Ok(p) => std::path::PathBuf::from(p),
        Err(_) => {
            let cwd_env = std::path::PathBuf::from(".env");
            if cwd_env.exists() {
                cwd_env
            } else {
                let data_env = crate::config::data_dir().join(".env");
                if data_env.exists() { data_env } else { cwd_env }
            }
        }
    };
    let mut preserved: Vec<String> = Vec::new();
    if env_path.exists() {
        let existing = match std::fs::read_to_string(&env_path) {
            Ok(s) => s,
            Err(e) => {
                return Html(html_page(
                    &state.base_path,
                    "Setup",
                    &format!(
                        "<p style=\"color:red\">Failed to read .env: {}</p>",
                        esc(&e.to_string())
                    ),
                ))
                .into_response();
            }
        };
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
    content.push_str(&format!("TELEGRAM_BOT_TOKEN={}\n", token));
    content.push_str(&format!("OWNER_ID={}\n", owner_id));
    content.push_str(&format!("DATABASE_URL=sqlite:{}\n", state.cfg.db_path));
    if let Some(c) = &channel {
        content.push_str(&format!("DEFAULT_CHANNEL={}\n", c));
    } else {
        content.push_str("DEFAULT_CHANNEL=\n");
    }
    if let Some(c) = &crypto {
        content.push_str(&format!("CRYPTO_PAY_TOKEN={}\n", c));
    } else {
        content.push_str("CRYPTO_PAY_TOKEN=\n");
    }
    content.push_str(&format!("WEB_USERNAME={}\n", uname));
    content.push_str(&format!("WEB_PASSWORD={}\n", pw));
    content.push_str(&format!("WEB_PORT={}\n", state.cfg.web_port()));
    content.push_str("RUST_LOG=info\n");
    if !preserved.is_empty() {
        content.push_str(&preserved.join("\n"));
        content.push('\n');
    }
    if let Err(e) = std::fs::write(&env_path, &content) {
        return Html(html_page(
            &state.base_path,
            "Setup",
            &format!(
                "<p style=\"color:red\">Failed to write .env: {}</p>",
                esc(&e.to_string())
            ),
        ))
        .into_response();
    }
    // best-effort permissions 0600
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o600));
    }

    // Seed owner web account + channel in DB.
    if state.db.get_web_account_by_username(&uname).is_none() {
        let _ = state.db.create_web_account(&uname, &pw, "owner", None);
    }
    if let Some(c) = &channel {
        state.db.set_config("channel", c);
    }

    // Mark setup complete in the DB (authoritative across restarts and for
    // the still-running process whose env snapshot is frozen).
    state.db.set_config("setup_done", "1");

    let body = format!(
        r#"<article><h2>Setup complete</h2>
<p>Configuration saved to <code>.env</code>. Restart the application to start the bot with the new settings.</p>
<p>If running under Docker/systemd, the service will restart automatically; otherwise restart manually.</p>
<a href="{}">Go to login</a></article>"#,
        state.url("/login")
    );
    Html(html_page(&state.base_path, "Setup", &body)).into_response()
}

async fn validate_bot_token(token: &str) -> bool {
    let url = format!("https://api.telegram.org/bot{}/getMe", token);
    match reqwest::Client::new().get(&url).send().await {
        Ok(resp) => {
            if let Ok(v) = resp.json::<serde_json::Value>().await {
                v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false)
            } else {
                false
            }
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use proptest::prelude::*;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tower::ServiceExt;

    proptest! {
        /// esc() output contains no raw HTML metacharacters (`< > " '` — the
        /// escaped forms use entities), every `&` in the output begins one of
        /// the five known entities, and decoding exactly restores the input.
        #[test]
        fn prop_esc_neutralizes_and_roundtrips(s in ".*") {
            let e = esc(&s);
            for c in ['<', '>', '"', '\''] {
                prop_assert!(!e.contains(c), "raw {c} survived escaping");
            }
            let entities = ["&amp;", "&lt;", "&gt;", "&quot;", "&#39;"];
            for (i, _) in e.match_indices('&').collect::<Vec<_>>() {
                let rest = &e[i..];
                prop_assert!(
                    entities.iter().any(|ent| rest.starts_with(ent)),
                    "stray & at byte {i} in {e:?}"
                );
            }
            // Decode order: &amp; LAST — restoring & early would fabricate
            // new entity sequences (esc("&#39;") == "&amp;#39;").
            let decoded = e
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&quot;", "\"")
                .replace("&#39;", "'")
                .replace("&amp;", "&");
            prop_assert_eq!(decoded, s);
        }
    }

    fn make_state() -> (WebState, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Db::new(db_path.to_str().unwrap()).unwrap();
        db.create_web_account("owner", "secret", "owner", None)
            .unwrap();
        let cfg = Config {
            bot_token: "0:dummy".to_string(),
            owner_id: 1,
            db_path: db_path.to_str().unwrap().to_string(),
            default_channel: None,
            crypto_token: None,
            ai_enabled: false,
            ai_provider: "openai".to_string(),
            ai_base_url: "https://api.openai.com/v1".to_string(),
            ai_api_key: None,
            ai_model: "gpt-4o-mini".to_string(),
            ai_daily_token_budget: None,
            mcp_owner_token: None,
        };
        let bot = teloxide::Bot::new(cfg.bot_token.clone());
        let state = WebState {
            db: Arc::new(db),
            cfg,
            bot,
            login_attempts: Arc::new(StdMutex::new(HashMap::new())),
            base_path: String::new(),
        };
        (state, dir)
    }

    async fn body_string(resp: axum::http::Response<Body>) -> String {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8_lossy(&bytes).to_string()
    }

    #[tokio::test]
    async fn unauth_dashboard_redirects_to_login() {
        let (state, _dir) = make_state();
        let app = router(state);
        let req = Request::builder()
            .uri("/dashboard")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(resp.status().is_redirection());
        assert_eq!(resp.headers().get("location").unwrap(), "/login");
    }

    #[tokio::test]
    async fn login_valid_sets_cookie_and_redirects() {
        let (state, _dir) = make_state();
        let app = router(state);
        let req = Request::builder()
            .method("POST")
            .uri("/login")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("username=owner&password=secret"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(resp.status().is_redirection());
        assert_eq!(resp.headers().get("location").unwrap(), "/dashboard");
        let set_cookie = resp.headers().get("set-cookie").unwrap().to_str().unwrap();
        assert!(set_cookie.contains("session="));
    }

    #[tokio::test]
    async fn login_invalid_returns_error() {
        let (state, _dir) = make_state();
        let app = router(state);
        let req = Request::builder()
            .method("POST")
            .uri("/login")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("username=owner&password=wrong"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;
        assert!(body.contains("Invalid credentials"));
    }

    #[tokio::test]
    async fn auth_dashboard_returns_200() {
        let (state, _dir) = make_state();
        let db = state.db.clone();
        let (acc_id, _u, _h, _r, _s) = db.get_web_account_by_username("owner").unwrap();
        let token = db.create_web_session(acc_id, "owner", None, 7);
        let app = router(state);
        let req = Request::builder()
            .uri("/dashboard")
            .header("cookie", format!("session={}", token))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn logout_clears_session() {
        let (state, _dir) = make_state();
        let db = state.db.clone();
        let (acc_id, _u, _h, _r, _s) = db.get_web_account_by_username("owner").unwrap();
        let token = db.create_web_session(acc_id, "owner", None, 7);
        let app = router(state);
        let req = Request::builder()
            .uri("/logout")
            .header("cookie", format!("session={}", token))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(resp.status().is_redirection());
        let set_cookie = resp.headers().get("set-cookie").unwrap().to_str().unwrap();
        assert!(set_cookie.contains("Max-Age=0"));
        assert!(db.get_web_session(&token).is_none());
    }
}
