use axum::{
    Router,
    extract::{Form, Path, Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{Html, IntoResponse, Json, Redirect, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
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
        .route("/assets/app.css", get(app_css))
        .route("/assets/fonts/IBMPlexSans-var.woff2", get(font_sans))
        .route("/assets/fonts/IBMPlexMono-400.woff2", get(font_mono_400))
        .route("/assets/fonts/IBMPlexMono-500.woff2", get(font_mono_500))
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

// ---- Request timeout (control-plane hardening, L8; zero new deps) ----

/// Production ceiling for any request on the merged router. Generous by
/// design: dashboard/API/MCP handlers do SQLite writes and awaited Telegram
/// sends (each with its own client timeout), the bot long-poll runs in the
/// dispatcher task — never through axum — and the cryptopay webhook only
/// performs two DB writes, so nothing legitimate needs 30s.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout middleware applied in `main.rs` around the merged router. On
/// elapse the handler future is dropped (its side effects so far stand —
/// SQLite writes are transactional) and a 503 in the API error envelope is
/// returned.
pub async fn request_timeout(req: Request, next: Next) -> Response {
    run_with_timeout(REQUEST_TIMEOUT, req, next).await
}

/// Parameterized core so tests exercise the real logic with a short limit
/// instead of sleeping 30s.
pub(crate) async fn run_with_timeout(limit: Duration, req: Request, next: Next) -> Response {
    match tokio::time::timeout(limit, next.run(req)).await {
        Ok(resp) => resp,
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": {"code": "timeout", "message": "request exceeded the time limit and was aborted"}
            })),
        )
            .into_response(),
    }
}

/// The design system, shipped from the binary (no CDN, no build step).
const APP_CSS: &str = include_str!("../assets/app.css");
const FONT_SANS: &[u8] = include_bytes!("../assets/fonts/IBMPlexSans-var.woff2");
const FONT_MONO_400: &[u8] = include_bytes!("../assets/fonts/IBMPlexMono-400.woff2");
const FONT_MONO_500: &[u8] = include_bytes!("../assets/fonts/IBMPlexMono-500.woff2");

async fn app_css() -> Response {
    asset_css()
}
async fn font_sans() -> Response {
    asset_font(FONT_SANS)
}
async fn font_mono_400() -> Response {
    asset_font(FONT_MONO_400)
}
async fn font_mono_500() -> Response {
    asset_font(FONT_MONO_500)
}

fn asset_css() -> Response {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS).into_response()
}

fn asset_font(body: &'static [u8]) -> Response {
    (
        [
            (header::CONTENT_TYPE, "font/woff2"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        body,
    )
        .into_response()
}

fn html_page(base: &str, title: &str, body: &str) -> String {
    format!(
        r#"<!DOCTYPE html><html lang="en" data-theme="light"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{t} — Telegram Business</title>
<link rel="stylesheet" href="{base}/assets/app.css">
<link rel="preload" href="{base}/assets/fonts/IBMPlexSans-var.woff2" as="font" type="font/woff2" crossorigin>
<script>(function(){{var s=localStorage.getItem('theme');var d=window.matchMedia('(prefers-color-scheme: dark)').matches;document.documentElement.setAttribute('data-theme',s||(d?'dark':'light'));}})();</script>
</head><body>
<div class="shell">
<aside class="rail">
  <div class="brand">{plane}<b>Telegram Business<small>operator console</small></b></div>
  <div class="group">Operate</div>
  <nav>{nav}</nav>
  <div class="foot"><button type="button" class="btn btn-ghost btn-sm theme-toggle" onclick="toggleTheme()">Theme</button></div>
</aside>
<main class="main"><div class="measure">
{b}
</div></main>
</div>
<script>function toggleTheme(){{var c=document.documentElement.getAttribute('data-theme');var n=c==='dark'?'light':'dark';document.documentElement.setAttribute('data-theme',n);localStorage.setItem('theme',n);}}</script>
</body></html>"#,
        base = base,
        t = title,
        b = body,
        plane = PLANE_SVG,
        nav = rail_nav(base, title)
    )
}

/// Front-door shell for pre-auth pages (setup / login): no rail, a focused
/// panel on a calm canvas, brand on top. The operator's first minute should
/// be one action wide, not a dashboard pretending to be configured.
fn html_page_bare(base: &str, title: &str, body: &str) -> String {
    format!(
        r#"<!DOCTYPE html><html lang="en" data-theme="light"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{t} — Telegram Business</title>
<link rel="stylesheet" href="{base}/assets/app.css">
<script>(function(){{var s=localStorage.getItem('theme');var d=window.matchMedia('(prefers-color-scheme: dark)').matches;document.documentElement.setAttribute('data-theme',s||(d?'dark':'light'));}})();</script>
</head><body>
<div class="door">
  <div class="brand">{plane}<b>Telegram Business</b></div>
  <div class="panel">{b}</div>
</div>
</body></html>"#,
        base = base,
        t = title,
        b = body,
        plane = PLANE_SVG
    )
}

/// Brand mark: a paper plane, drawn with three strokes — the same shape the
/// product's world (Telegram) speaks in, reduced to the console's own line weight.
const PLANE_SVG: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M21 3.5 3.8 10.2c-.8.3-.8 1.4 0 1.7l4.9 1.8 1.8 4.9c.3.8 1.4.8 1.7 0L19 5.5"/><path d="M10.5 13.7 21 3.5"/></svg>"#;

/// Rail nav with the current page marked. The active entry is derived from
/// the page title (they are the same vocabulary on purpose — one name per
/// place, per DESIGN.md's voice rules).
fn rail_nav(base: &str, title: &str) -> String {
    let items = [
        ("dashboard", "Dashboard"),
        ("channels", "Channels"),
        ("plans", "Plans"),
        ("products", "Products"),
        ("posts", "Posts"),
    ];
    let mut out = String::new();
    for (path, label) in items {
        let active = if title.eq_ignore_ascii_case(label) {
            " class=\"active\""
        } else {
            ""
        };
        out.push_str(&format!("<a href=\"{base}/{path}\"{active}>{label}</a>"));
    }
    out.push_str(&format!("<a href=\"{base}/logout\">Log out</a>"));
    out
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
    Html(html_page_bare(
        &state.base_path,
        "Login",
        &format!(
            r#"<h2>Sign in</h2>
<p class="help">The operator console for your Telegram shop.</p>
<form method="post" action="{}">
<label>Username<input type="text" name="username" required autocomplete="username"></label>
<label>Password<input type="password" name="password" required autocomplete="current-password"></label>
<button type="submit">Sign in</button>
</form>"#,
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
            Html(html_page_bare(
                &state.base_path,
                "Login",
                r##"<div class="alert alert-error">Too many attempts. Try again in a few minutes.</div>
<p><a href="#">Back to sign in</a></p>"##,
            )),
        )
            .into_response();
    }
    if let Some((account_id, role, seller_id)) =
        state.db.verify_web_password(&f.username, &f.password).await
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
        Html(html_page_bare(
            &state.base_path,
            "Login",
            &format!(
                r#"<div class="alert alert-error">Invalid credentials. Check the username and password, then try again.</div>
<form method="post" action="{}">
<label>Username<input type="text" name="username" required autocomplete="username"></label>
<label>Password<input type="password" name="password" required autocomplete="current-password"></label>
<button type="submit">Sign in</button>
</form>"#,
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
    let plans_list = if plans.is_empty() {
        "<li class=\"empty\"><b>No plans yet.</b>Create your first plan below — buyers see it in the shop.</li>"
            .to_string()
    } else {
        plans
            .iter()
            .map(|p| {
                format!(
                    "<li><b>{}</b><span class=\"mono\">{} {}</span><span class=\"help\">/ {} days</span></li>",
                    esc(&p.0),
                    esc(&p.1),
                    esc(&p.2),
                    p.3
                )
            })
            .collect::<Vec<_>>()
            .join("")
    };
    let subs_list = if subs.is_empty() {
        "<li class=\"empty\"><b>No subscriptions yet.</b>They appear here the moment a buyer activates.</li>"
            .to_string()
    } else {
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
            .join("")
    };
    let body = format!(
        r#"<h1>Dashboard <span class="sub">({})</span></h1>
<article class="panel"><h3>Default channel</h3><p class="mono">{}</p></article>
<div class="grid-2">
<article class="panel"><h3>Plans ({})</h3><ul>{}</ul></article>
<article class="panel"><h3>Recent subscriptions ({})</h3><ul>{}</ul></article>
</div>
<div class="grid-2">
<article class="panel"><h3>Create plan</h3><form method="post" action="{}">
<label>Name<input name="name" required></label>
<label>Price<input name="price" required></label>
<label>Asset<input name="asset" required></label>
<label>Duration days<input name="duration_days" type="number" required></label>
<label>Channel<input name="channel" placeholder="@channel (optional)"></label>
<button type="submit">Create plan</button></form></article>
<article class="panel"><h3>Post</h3><form method="post" action="{}">
<label>Channel<input name="channel" placeholder="@channel (optional)"></label>
<label>Text<textarea name="text" required></textarea></label>
<button type="submit">Post</button></form></article>
</div>"#,
        esc(&session.role),
        esc(&ch),
        plans.len(),
        plans_list,
        subs.len(),
        subs_list,
        state.url("/plan"),
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
    let list = if set.is_empty() {
        "<li class=\"empty\"><b>No channels yet.</b>Channels appear here as you add plans and set a default channel.</li>"
            .to_string()
    } else {
        set.iter()
            .map(|c| format!("<li><span class=\"mono\">{}</span></li>", esc(c)))
            .collect::<Vec<_>>()
            .join("")
    };
    let body = format!(
        "<h1>Channels <span class=\"sub\">known to this console</span></h1><article class=\"panel\"><ul>{}</ul></article>",
        list
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
    let list_items = if plans.is_empty() {
        "<li class=\"empty\"><b>No plans yet.</b>Create one below — it appears in the shop immediately.</li>"
            .to_string()
    } else {
        plans
            .iter()
            .map(|p| {
                format!(
                    "<li><b>{}</b><span class=\"mono\">{} {}</span><span class=\"help\">/ {} days</span><span class=\"row-action\"><form method=\"post\" action=\"{}\" class=\"inline-form\"><input type=\"hidden\" name=\"name\" value=\"{}\"><button type=\"submit\" class=\"btn btn-danger btn-sm\">Delete</button></form></span></li>",
                    esc(&p.0),
                    esc(&p.1),
                    esc(&p.2),
                    p.3,
                    state.url("/plandel"),
                    esc(&p.0)
                )
            })
            .collect::<Vec<_>>()
            .join("")
    };
    let body = format!(
        r#"<h1>Plans <span class="sub">what buyers can purchase</span></h1>
<article class="panel"><ul>{}</ul></article>
<article class="panel"><h3>Create plan</h3><form method="post" action="{}">
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
                    "<tr><td><b>{}</b></td><td class=\"num\">{}</td><td class=\"mono\">{}</td><td>{}</td><td class=\"num\">{}</td><td>{}</td><td class=\"actions\"><form method=\"post\" action=\"{}\" class=\"inline-form\"><input type=\"hidden\" name=\"name\" value=\"{}\"><button type=\"submit\" class=\"btn btn-danger btn-sm\">Deactivate</button></form></td></tr>",
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
            "<div class=\"table-wrap\"><table><thead><tr><th>Name</th><th class=\"num\">Price</th><th>Asset</th><th>Kind</th><th class=\"num\">Stock</th><th>Active</th><th></th></tr></thead><tbody>{}</tbody></table></div>",
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
                r#"<div class="alert alert-error">{}</div><p><a href="{}">Back to products</a></p>"#,
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
        r#"<h2>Welcome — first-run setup</h2>
<p class="help">Connect your bot and claim the console. Takes about two minutes; every field has a hint below.</p>
<details open><summary>How to get a bot token (2 minutes, no skills needed)</summary>
<ol><li>Open Telegram and message <a href="https://t.me/BotFather" rel="noopener">@BotFather</a>.</li>
<li>Send <code>/newbot</code>, choose a display name, then a username ending in <code>bot</code>.</li>
<li>Copy the token it replies with (<code>123456:ABC-…</code>) and paste it below.</li>
<li>For your owner ID, message <a href="https://t.me/userinfobot" rel="noopener">@userinfobot</a> — it replies with your numeric ID.</li>
<li>Keep the bot's privacy mode ON; add it as <b>admin</b> to your channel when you sell access.</li></ol></details>
<form method="post" action="{}">
<label>Bot token<input type="text" name="bot_token" required autocomplete="off" placeholder="123456:ABC-DEF…"><span class="help">From @BotFather. Stored in .env with 0600 permissions, never logged.</span></label>
<label>Owner Telegram user ID<input type="text" name="owner_id" required inputmode="numeric" placeholder="123456789"><span class="help">Your numeric ID from @userinfobot — this account owns the console.</span></label>
<label>Default channel<input type="text" name="default_channel" placeholder="@yourchannel"><span class="help">Optional — where posts and grants go when nothing else is set.</span></label>
<label>Crypto Pay token<input type="text" name="crypto_token" autocomplete="off" placeholder="from @CryptoBot → Crypto Pay → Create App"><span class="help">Optional — enables automated payment verification.</span></label>
<div class="form-row">
<div><label>Dashboard username<input type="text" name="web_username" value="owner" autocomplete="username"></label></div>
<div><label>Dashboard password<input type="password" name="web_password" required minlength="6" autocomplete="new-password"><span class="help">At least 6 characters — this unlocks the console.</span></label></div>
</div>
<button type="submit">Save and continue</button>
</form>"#,
        state.url("/setup")
    );
    Html(html_page_bare(&state.base_path, "Setup", &body)).into_response()
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
        return setup_error(&state, "Bot token is required.");
    }
    let ok = validate_bot_token(&token).await;
    if !ok {
        return setup_error(&state, "Invalid bot token (Telegram rejected it).");
    }
    let owner_id: i64 = match f.owner_id.trim().parse() {
        Ok(v) if v > 0 => v,
        _ => {
            return setup_error(&state, "Owner ID must be a positive number.");
        }
    };
    let uname = f
        .web_username
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "owner".to_string());
    let pw = f.web_password.clone().unwrap_or_default();
    if pw.len() < 6 {
        return setup_error(&state, "Password must be at least 6 characters.");
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
                return setup_error(
                    &state,
                    &format!("Failed to read .env: {}", esc(&e.to_string())),
                );
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
        return setup_error(
            &state,
            &format!("Failed to write .env: {}", esc(&e.to_string())),
        );
    }
    // best-effort permissions 0600
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o600));
    }

    // Seed owner web account + channel in DB.
    if state.db.get_web_account_by_username(&uname).is_none() {
        let _ = state
            .db
            .create_web_account(&uname, &pw, "owner", None)
            .await;
    }
    if let Some(c) = &channel {
        state.db.set_config("channel", c);
    }

    // Mark setup complete in the DB (authoritative across restarts and for
    // the still-running process whose env snapshot is frozen).
    state.db.set_config("setup_done", "1");

    let body = format!(
        r#"<div class="alert alert-success">Setup complete. Configuration saved to <code>.env</code>.</div>
<p>Restart the application to start the bot with the new settings. If it runs under Docker or systemd the service restart handles it; otherwise start it again manually.</p>
<p><a class="btn" href="{}">Go to sign in</a></p>"#,
        state.url("/login")
    );
    Html(html_page_bare(&state.base_path, "Setup", &body)).into_response()
}

/// Pre-setup error surface: the front-door shell with an honest alert and a
/// way back. Every setup failure says what happened and what to do next.
fn setup_error(state: &WebState, msg: &str) -> Response {
    Html(html_page_bare(
        &state.base_path,
        "Setup",
        &format!(
            r#"<div class="alert alert-error">{}</div><p><a href="{}">Back to setup</a></p>"#,
            msg,
            state.url("/setup")
        ),
    ))
    .into_response()
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

    async fn make_state() -> (WebState, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Db::new(db_path.to_str().unwrap()).unwrap();
        db.create_web_account("owner", "secret", "owner", None)
            .await
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
        let (state, _dir) = make_state().await;
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
        let (state, _dir) = make_state().await;
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
        let (state, _dir) = make_state().await;
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
        let (state, _dir) = make_state().await;
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
        let (state, _dir) = make_state().await;
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

#[cfg(test)]
mod timeout_tests {
    // Timeout middleware tests (L8). The production ceiling is 30s; the
    // tests exercise the real `run_with_timeout` logic with a short limit so
    // nothing sleeps 30 seconds. No sleep route exists in production code —
    // these live only inside this test module.
    use super::*;
    use axum::body::Body;
    use axum::routing::get;
    use tower::ServiceExt;

    async fn slow() -> &'static str {
        tokio::time::sleep(Duration::from_millis(500)).await;
        "done"
    }
    async fn fast() -> &'static str {
        "ok"
    }

    fn app(limit: Duration) -> Router {
        Router::new()
            .route("/slow", get(slow))
            .route("/fast", get(fast))
            .layer(axum::middleware::from_fn(move |req, next| async move {
                run_with_timeout(limit, req, next).await
            }))
    }

    #[tokio::test]
    async fn timeout_returns_503_envelope() {
        let resp = app(Duration::from_millis(50))
            .oneshot(
                axum::http::Request::builder()
                    .uri("/slow")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["code"], "timeout");
    }

    #[tokio::test]
    async fn fast_handlers_pass_through_untimed() {
        let resp = app(Duration::from_millis(2000))
            .oneshot(
                axum::http::Request::builder()
                    .uri("/fast")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
