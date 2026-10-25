use axum::{
    Router,
    extract::{Form, FromRequestParts, Path, Query, Request, State, connect_info::ConnectInfo},
    http::{StatusCode, header},
    middleware::Next,
    response::{Html, IntoResponse, Json, Redirect, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use teloxide::{Bot, prelude::*};
use tower_http::services::ServeDir;

use crate::{config::Config, db::Db, util::ct_eq};

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

/// The peer IP of the request, when the server tracks one. `main.rs` serves
/// with `into_make_service_with_connect_info::<SocketAddr>()`, which injects
/// the connection info as a request extension before routing; this extractor
/// reads it back and NEVER rejects — absent (oneshot tests, or a server that
/// does not track peers) means unknown, and security decisions treat unknown
/// as remote.
pub struct PeerIp(pub Option<IpAddr>);

impl FromRequestParts<WebState> for PeerIp {
    type Rejection = std::convert::Infallible;

    fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &WebState,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        // Read the extension before returning the future so the borrow of
        // `parts` ends here.
        let ip = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip());
        std::future::ready(Ok(PeerIp(ip)))
    }
}

impl PeerIp {
    /// True only when the request provably arrived from the machine itself.
    /// `None` (peer unknown) is deliberately NOT loopback: absence of
    /// evidence must never widen the claim window.
    fn is_loopback(&self) -> bool {
        matches!(self.0, Some(ip) if ip.is_loopback())
    }
}

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

pub fn router(state: WebState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/assets/app.css", get(app_css))
        .route("/assets/fonts/IBMPlexSans-var.woff2", get(font_sans))
        .route("/assets/fonts/IBMPlexMono-400.woff2", get(font_mono_400))
        .route("/assets/fonts/IBMPlexMono-500.woff2", get(font_mono_500))
        .route("/setup", get(setup_page).post(setup_submit))
        .route("/setup/skip", post(setup_skip))
        .route("/login", get(login_page).post(login_submit))
        .route("/logout", get(logout))
        .route("/dashboard", get(dashboard))
        .route("/settings", get(settings_page))
        .route("/settings/save", post(settings_save))
        .route("/settings/password", post(settings_password))
        .route("/channels", get(channels_page))
        .route("/channels/add", post(channels_add))
        .route("/channels/default", post(channels_default))
        .route("/channels/remove", post(channels_remove))
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
  <div class="foot"><a href="{base}/settings">Settings</a> <button type="button" class="btn btn-ghost btn-sm theme-toggle" onclick="toggleTheme()">Theme</button></div>
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

/// Query shared by the login GET and POST: a `?code=` deep link (printed to
/// the server console once at startup) is how a remote operator reaches the
/// claim form without being on the machine.
#[derive(Deserialize)]
struct ClaimQuery {
    code: Option<String>,
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
    /// Claim mode only: the confirmation retype. Server-side checked — the
    /// browser's minlength is a courtesy, never the gate.
    confirm_password: Option<String>,
    /// Claim mode only: the one-time code, either as a form field or via
    /// `?code=` (see [`ClaimQuery`]). Never rendered back on failure.
    claim_code: Option<String>,
}

/// The sign-in form. `alert` (already-escaped or static text) is shown above
/// it — used by the GET page and both failure branches so the three surfaces
/// stay one form, one vocabulary.
fn sign_in_form_html(state: &WebState, alert: Option<&str>) -> String {
    let alert = alert
        .map(|a| format!(r#"<div class="alert alert-error">{}</div>"#, a))
        .unwrap_or_default();
    format!(
        r#"{alert}<h2>Sign in</h2>
<p class="help">The operator console for your Telegram shop.</p>
<form method="post" action="{}">
<label>Username<input type="text" name="username" required autocomplete="username"></label>
<label>Password<input type="password" name="password" required autocomplete="current-password"></label>
<button type="submit">Sign in</button>
</form>"#,
        state.url("/login")
    )
}

/// The claim form: the front door while no owner account exists. `provided`
/// is the caller-SUPPLIED code (from `?code=` or a previous attempt) carried
/// in a hidden field so a remote operator who mistypes their password keeps
/// their code; the server's real code is never echoed anywhere.
fn claim_form_html(state: &WebState, alert: Option<&str>, provided: Option<&str>) -> String {
    let alert = alert
        .map(|a| format!(r#"<div class="alert alert-error">{}</div>"#, a))
        .unwrap_or_default();
    let hidden = provided
        .map(|c| {
            format!(
                r#"<input type="hidden" name="claim_code" value="{}">"#,
                esc(c)
            )
        })
        .unwrap_or_default();
    format!(
        r#"{alert}<h2>Claim the console</h2>
<p class="help">No dashboard account exists yet. The account you create here becomes the owner — it manages settings, channels and sellers. This offer ends once an owner exists.</p>
<form method="post" action="{}">
{hidden}
<label>Username<input type="text" name="username" required autocomplete="username"></label>
<label>Password<input type="password" name="password" required minlength="6" autocomplete="new-password"></label>
<label>Confirm password<input type="password" name="confirm_password" required minlength="6" autocomplete="new-password"><span class="help">At least 6 characters, typed twice.</span></label>
<button type="submit">Claim the console</button>
</form>"#,
        state.url("/login"),
        hidden = hidden,
    )
}

async fn login_page(
    State(state): State<WebState>,
    Query(q): Query<ClaimQuery>,
) -> impl IntoResponse {
    if !setup_done(&state) {
        return Redirect::to(&state.url("/setup")).into_response();
    }
    if !state.db.owner_account_exists() {
        // Claim mode: the front door IS the claim form.
        return Html(html_page_bare(
            &state.base_path,
            "Claim",
            &claim_form_html(&state, None, q.code.as_deref()),
        ))
        .into_response();
    }
    Html(html_page_bare(
        &state.base_path,
        "Login",
        &sign_in_form_html(&state, None),
    ))
    .into_response()
}

async fn login_submit(
    State(state): State<WebState>,
    peer: PeerIp,
    Query(q): Query<ClaimQuery>,
    Form(f): Form<LoginForm>,
) -> impl IntoResponse {
    // Claim mode: with no owner account, POST /login IS the claim submission.
    // Once an owner exists this branch can never run again (claim mode never
    // returns), so normal sign-in below stays the steady state.
    if !state.db.owner_account_exists() {
        return claim_submit(&state, &peer, &q, &f).await;
    }
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
        (
            [(header::SET_COOKIE, session_cookie(&state, &token))],
            Redirect::to(&state.url("/dashboard")),
        )
            .into_response()
    } else {
        // The failure was already recorded by `login_throttle` above.
        Html(html_page_bare(
            &state.base_path,
            "Login",
            &sign_in_form_html(
                &state,
                Some("Invalid credentials. Check the username and password, then try again."),
            ),
        ))
        .into_response()
    }
}

/// Anti-takeover gate for the claim flow (PLAN v1.3.x). Claiming is allowed
/// from loopback — the desktop default bind is 127.0.0.1, where claiming with
/// no code is the whole point — or with the one-time claim code generated at
/// startup, compared in constant time. Anything else is refused, and the
/// code itself is never rendered back.
fn claim_authorized(state: &WebState, peer: &PeerIp, provided: Option<&str>) -> bool {
    if peer.is_loopback() {
        return true;
    }
    match (
        provided.map(str::trim).filter(|c| !c.is_empty()),
        state.db.claim_code(),
    ) {
        (Some(code), Some(expected)) => ct_eq(code, &expected),
        _ => false,
    }
}

/// Claim-mode POST /login: create the owner account and sign it in, exactly
/// like a successful sign-in. Every refusal renders the claim form again —
/// with the caller's own code (if any) preserved in the hidden field.
async fn claim_submit(state: &WebState, peer: &PeerIp, q: &ClaimQuery, f: &LoginForm) -> Response {
    // Same throttle as sign-in: a claim endpoint that guesses passwords at
    // full speed is just a login form with worse hygiene.
    if login_throttle(&state.login_attempts, &f.username, 5, 900) {
        return claim_error(
            state,
            None,
            "Too many attempts. Try again in a few minutes.",
        );
    }
    // The gate runs before any account work: a remote request without a
    // valid code learns nothing except that the console is unclaimed —
    // which the public setup-skip flow already reveals.
    let provided = q.code.as_deref().or(f.claim_code.as_deref());
    if !claim_authorized(state, peer, provided) {
        return claim_error(
            state,
            provided,
            "Claiming requires the one-time code shown in the server console.",
        );
    }
    let uname = f.username.trim();
    let pw = f.password.as_str();
    if uname.is_empty() {
        return claim_error(state, provided, "Choose a username.");
    }
    if pw.chars().count() < 6 {
        return claim_error(state, provided, "Password must be at least 6 characters.");
    }
    if f.confirm_password.as_deref() != Some(pw) {
        return claim_error(state, provided, "Passwords do not match.");
    }
    if state.db.get_web_account_by_username(uname).is_some() {
        return claim_error(state, provided, "That username is taken.");
    }
    if let Err(e) = state.db.create_web_account(uname, pw, "owner", None).await {
        return claim_error(
            state,
            provided,
            &format!("Could not create the account: {}", esc(&e)),
        );
    }
    login_ok(&state.login_attempts, uname);
    // The freshly created account is the owner session; mirror login_submit's
    // cookie contract exactly (same lifetime, same flags).
    let (account_id, _u, _hash, _role, _seller) = state
        .db
        .get_web_account_by_username(uname)
        .expect("account was just created");
    let token = state.db.create_web_session(account_id, "owner", None, 7);
    (
        [(header::SET_COOKIE, session_cookie(state, &token))],
        Redirect::to(&state.url("/dashboard")),
    )
        .into_response()
}

fn claim_error(state: &WebState, provided: Option<&str>, msg: &str) -> Response {
    Html(html_page_bare(
        &state.base_path,
        "Claim",
        &claim_form_html(state, Some(msg), provided),
    ))
    .into_response()
}

/// The `session=<token>` cookie for a fresh login, shared by sign-in and
/// claim so both doors hand out identical cookies.
fn session_cookie(state: &WebState, token: &str) -> String {
    if secure_cookies() {
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
    // Default-channel reader: resolve through the managed channels table
    // with the legacy-config fallback (matches Db::default_channel's other
    // call sites, e.g. api.rs posts_create).
    let ch = state
        .db
        .default_channel()
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
    // Persistent honesty: an unconfigured console says so on every visit —
    // the bot is not running, and here is the one place that fixes it.
    let connect_banner = if state.cfg.is_configured() {
        String::new()
    } else {
        format!(
            r#"<div class="alert alert-info">The bot is not connected yet. Add your bot token in <a href="{}">Settings</a> — it connects on the next restart.</div>"#,
            state.url("/settings")
        )
    };
    // The Post form offers exactly the channels this session may post to:
    // list_channels(seller_scope) — the global set plus the seller's own for
    // a seller, every known row for the owner. Dropdown only — the free-text
    // guess moved to the API/MCP path, which still accepts arbitrary
    // channels.
    let mut channel_options = String::from("<option value=\"\">(default)</option>");
    let mut seen: Vec<String> = Vec::new();
    for row in state.db.list_channels(seller_scope) {
        seen.push(row.channel.clone());
        channel_options.push_str(&format!(
            "<option value=\"{0}\">{0}</option>",
            esc(&row.channel)
        ));
    }
    // A seller's bot-managed channel lives in the legacy sellers column
    // (set_seller_channel — what /setchannel writes), which list_channels
    // does not read. Merge it so the seller can actually select their own
    // posting target instead of guessing a table row they do not have.
    if let Some(sid) = seller_scope
        && let Some(own) = state.db.seller_channel(sid)
        && !seen.iter().any(|c| c.eq_ignore_ascii_case(&own))
    {
        channel_options.push_str(&format!("<option value=\"{0}\">{0}</option>", esc(&own)));
    }
    let body = format!(
        r#"<h1>Dashboard <span class="sub">({})</span></h1>
{connect_banner}
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
<label>Channel<select name="channel">{}</select><span class="help">Leave on (default) to post where the default channel points.</span></label>
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
        channel_options,
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
    Html(html_page(
        &state.base_path,
        "Channels",
        &channels_body(&state, &session, None),
    ))
    .into_response()
}

/// The channels manager body (PLAN v1.3.x). The v1 contract recognizes
/// exactly two classes: the owner manages the global set; a seller sees only
/// their own bot-managed channel. `alert` renders an inline error so a
/// failed action can come straight back to the page (PRG on success only).
fn channels_body(state: &WebState, session: &Session, alert: Option<&str>) -> String {
    let alert = alert
        .map(|a| format!(r#"<div class="alert alert-error">{}</div>"#, a))
        .unwrap_or_default();
    if session.role == "seller" {
        // The seller's bot-managed channel is a single setting (the legacy
        // sellers column, what /setchannel writes) — shown read-only here,
        // replaced by the form below. Table rows a seller owns would be
        // managed by the scoped add/remove handlers, not this panel.
        let list = match state.db.seller_channel(session.seller_id.unwrap_or(0)) {
            Some(ch) => format!(
                "<li><span class=\"mono\">{}</span><span class=\"badge badge-success\">bot-managed</span></li>",
                esc(&ch)
            ),
            None => "<li class=\"empty\"><b>No channel yet.</b>Add one below, or use /setchannel in the bot.</li>"
                .to_string(),
        };
        return format!(
            r#"{alert}<h1>Channels <span class="sub">your posting target</span></h1>
<article class="panel"><h3>Your channel</h3><ul>{list}</ul>
<form method="post" action="{}">
<label>Channel<input name="channel" required placeholder="@yourchannel"><span class="help">@-prefixed Telegram channel. Adding one replaces the previous.</span></label>
<button type="submit">Set channel</button>
</form></article>"#,
            state.url("/channels/add"),
            alert = alert,
            list = list,
        );
    }
    // Global panel: full actions for the owner; other operator roles see the
    // rows read-only (the v1 contract gates mutations on the owner session).
    let is_owner = session.role == "owner";
    let rows = state
        .db
        .list_channels(None)
        .into_iter()
        .filter(|row| row.seller_id == 0)
        .map(|row| {
            let badge = if row.is_default {
                "<span class=\"badge badge-success\">default</span>".to_string()
            } else {
                String::new()
            };
            // Set default and Remove exist only on non-default rows — the
            // default channel is the fallback target and must not be
            // deletable from the row it anchors.
            let actions = if is_owner && !row.is_default {
                format!(
                    "<form method=\"post\" action=\"{}\" class=\"inline-form\"><input type=\"hidden\" name=\"channel\" value=\"{}\"><button type=\"submit\" class=\"btn btn-ghost btn-sm\">Set default</button></form><form method=\"post\" action=\"{}\" class=\"inline-form\"><input type=\"hidden\" name=\"channel\" value=\"{}\"><button type=\"submit\" class=\"btn btn-danger btn-sm\">Remove</button></form>",
                    state.url("/channels/default"),
                    esc(&row.channel),
                    state.url("/channels/remove"),
                    esc(&row.channel)
                )
            } else {
                String::new()
            };
            format!(
                "<li><span class=\"mono\">{}</span>{badge}<span class=\"row-action\">{actions}</span></li>",
                esc(&row.channel),
            )
        })
        .collect::<Vec<_>>()
        .join("");
    let list = if rows.is_empty() {
        "<li class=\"empty\"><b>No global channels yet.</b>Add one below — it becomes a posting target for plans and posts.</li>"
            .to_string()
    } else {
        rows
    };
    let add_form = if is_owner {
        format!(
            r#"<form method="post" action="{}">
<label>Channel<input name="channel" required placeholder="@yourchannel"><span class="help">@-prefixed Telegram channel. Adding one does not change the default — use Set default.</span></label>
<button type="submit">Add channel</button>
</form>"#,
            state.url("/channels/add")
        )
    } else {
        String::new()
    };
    format!(
        r#"{alert}<h1>Channels <span class="sub">the global posting targets</span></h1>
<article class="panel"><h3>Global channels</h3><ul>{list}</ul>
{add_form}
<p class="help">Sellers manage their own channel via /setchannel in the bot.</p>
</article>"#,
        alert = alert,
        list = list,
        add_form = add_form,
    )
}

/// Re-render the channels page with an inline error (no redirect: the failed
/// action's message belongs on the page it failed from).
fn channels_error(state: &WebState, session: &Session, msg: &str) -> Response {
    Html(html_page(
        &state.base_path,
        "Channels",
        &channels_body(state, session, Some(msg)),
    ))
    .into_response()
}

#[derive(Deserialize)]
struct ChannelForm {
    channel: String,
}

async fn channels_add(
    State(state): State<WebState>,
    headers: axum::http::HeaderMap,
    Form(f): Form<ChannelForm>,
) -> impl IntoResponse {
    let session = match session_from_headers(&state, &headers) {
        Some(s) => s,
        None => return Redirect::to(&state.url("/login")).into_response(),
    };
    let ch = f.channel.trim();
    if ch.is_empty() {
        return channels_error(&state, &session, "Type a channel name first.");
    }
    // Sellers adding = setting their own channel (the bot's /setchannel
    // semantics); owners add to the global set (seller_id 0).
    if session.role == "seller" {
        match session.seller_id {
            Some(sid) => {
                state.db.set_seller_channel(sid, ch);
                Redirect::to(&state.url("/channels")).into_response()
            }
            None => forbidden_page(&state, "Channels", "Your session has no seller profile."),
        }
    } else if session.role == "owner" {
        match state.db.add_channel(ch, 0) {
            Ok(()) => Redirect::to(&state.url("/channels")).into_response(),
            Err(e) => channels_error(&state, &session, &esc(&e)),
        }
    } else {
        forbidden_page(&state, "Channels", "Global channels")
    }
}

async fn channels_default(
    State(state): State<WebState>,
    headers: axum::http::HeaderMap,
    Form(f): Form<ChannelForm>,
) -> impl IntoResponse {
    let session = match session_from_headers(&state, &headers) {
        Some(s) => s,
        None => return Redirect::to(&state.url("/login")).into_response(),
    };
    if session.role != "owner" {
        return forbidden_page(&state, "Channels", "The default channel");
    }
    let ch = f.channel.trim();
    if ch.is_empty() {
        return channels_error(&state, &session, "Type a channel name first.");
    }
    // set_default_channel refuses seller-owned rows and unknown names —
    // surface that honestly instead of redirecting as if it worked.
    match state.db.set_default_channel(ch) {
        Ok(()) => Redirect::to(&state.url("/channels")).into_response(),
        Err(e) => channels_error(&state, &session, &esc(&e)),
    }
}

async fn channels_remove(
    State(state): State<WebState>,
    headers: axum::http::HeaderMap,
    Form(f): Form<ChannelForm>,
) -> impl IntoResponse {
    let session = match session_from_headers(&state, &headers) {
        Some(s) => s,
        None => return Redirect::to(&state.url("/login")).into_response(),
    };
    let ch = f.channel.trim();
    if ch.is_empty() {
        return channels_error(&state, &session, "Type a channel name first.");
    }
    // The default channel is the fallback posting target: removing it would
    // silently break every plan that relies on the fallback, so it is
    // refused until another channel takes over. The check is scope-aware —
    // seller rows are never the global default.
    if session.role == "owner" {
        if state.db.default_channel().as_deref() == Some(ch) {
            return channels_error(
                &state,
                &session,
                "The default channel cannot be removed — set another channel as default first.",
            );
        }
        match state.db.remove_channel(ch, 0) {
            Ok(()) => Redirect::to(&state.url("/channels")).into_response(),
            Err(e) => channels_error(&state, &session, &esc(&e)),
        }
    } else if session.role == "seller" {
        match session.seller_id {
            Some(sid) => match state.db.remove_channel(ch, sid) {
                Ok(()) => Redirect::to(&state.url("/channels")).into_response(),
                Err(e) => channels_error(&state, &session, &esc(&e)),
            },
            None => forbidden_page(&state, "Channels", "Your session has no seller profile."),
        }
    } else {
        forbidden_page(&state, "Channels", "Global channels")
    }
}

/// 403-style refusal page in the console shell: names what is restricted and
/// stops there — no partial content renders.
fn forbidden_page(state: &WebState, page: &str, what: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Html(html_page(
            &state.base_path,
            page,
            &format!(
                r#"<div class="alert alert-error">{} is restricted to the console owner.</div><p><a href="{}">Back to the dashboard</a></p>"#,
                esc(what),
                state.url("/dashboard")
            ),
        )),
    )
        .into_response()
}

// ---- Settings (owner-only, in-console configuration) ----

async fn settings_page(
    State(state): State<WebState>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let session = match session_from_headers(&state, &headers) {
        Some(s) => s,
        None => return Redirect::to(&state.url("/login")).into_response(),
    };
    if session.role != "owner" {
        return forbidden_page(&state, "Settings", "Settings");
    }
    Html(html_page(
        &state.base_path,
        "Settings",
        &settings_body(&state, None),
    ))
    .into_response()
}

/// The settings page body. `banner` carries the outcome of a save (rendered
/// above the panels so a POST can answer in place). Secrets are write-only:
/// the current bot token / Crypto Pay token are never rendered back, only
/// whether one is configured.
fn settings_body(state: &WebState, banner: Option<&str>) -> String {
    let banner = banner.unwrap_or_default();
    let token_badge = if state.cfg.bot_token.trim().is_empty() {
        r#"<span class="badge badge-warning">not connected</span>"#.to_string()
    } else {
        r#"<span class="badge badge-success">configured</span>"#.to_string()
    };
    let token_help = if state.cfg.bot_token.trim().is_empty() {
        "From @BotFather. The bot connects on restart once a token is saved."
    } else {
        "leave blank to keep the current token"
    };
    let crypto_badge = if state
        .cfg
        .crypto_token
        .as_deref()
        .unwrap_or("")
        .trim()
        .is_empty()
    {
        r#"<span class="badge badge-warning">not set</span>"#.to_string()
    } else {
        r#"<span class="badge badge-success">configured</span>"#.to_string()
    };
    let owner_id_value = if state.cfg.owner_id != 0 {
        state.cfg.owner_id.to_string()
    } else {
        String::new()
    };
    let default_channel_value = state.cfg.default_channel.clone().unwrap_or_default();
    format!(
        r#"{banner}<h1>Settings <span class="sub">bot connection and console access</span></h1>
<article class="panel"><h3>Bot connection</h3>
<form method="post" action="{}">
<label>Bot token {token_badge}<input type="password" name="bot_token" autocomplete="new-password" placeholder="123456:ABC-DEF…"><span class="help">{token_help}. Stored in .env with 0600 permissions and never rendered back.</span></label>
<label>Owner Telegram user ID<input type="text" name="owner_id" inputmode="numeric" value="{}" placeholder="123456789"><span class="help">Blank = keep current. Applies on restart — the running process keeps the snapshot it started with.</span></label>
<label>Default channel<input type="text" name="default_channel" value="{}" placeholder="@yourchannel"><span class="help">Blank = keep current. Where posts and grants go when nothing else is set.</span></label>
<label>Crypto Pay token {crypto_badge}<input type="password" name="crypto_token" autocomplete="new-password" placeholder="from @CryptoBot → Crypto Pay → Create App"><span class="help">leave blank to keep the current token. Enables automated payment verification.</span></label>
<button type="submit">Save settings</button>
</form>
<p class="help">Every field is blank-keeps-current: tabbing through without typing changes nothing. Saving writes .env only — the bot connects on restart.</p>
</article>
<article class="panel"><h3>Dashboard password</h3>
<form method="post" action="{}">
<label>New password<input type="password" name="new_password" required minlength="6" autocomplete="new-password"><span class="help">At least 6 characters. Other sessions of this account are signed out; this one stays.</span></label>
<button type="submit">Change password</button>
</form></article>"#,
        state.url("/settings/save"),
        esc(&owner_id_value),
        esc(&default_channel_value),
        state.url("/settings/password"),
        banner = banner,
        token_badge = token_badge,
        token_help = token_help,
        crypto_badge = crypto_badge,
    )
}

fn settings_response(state: &WebState, banner_html: String) -> Response {
    Html(html_page(
        &state.base_path,
        "Settings",
        &settings_body(state, Some(&banner_html)),
    ))
    .into_response()
}

fn settings_error(state: &WebState, msg: &str) -> Response {
    settings_response(
        state,
        format!(r#"<div class="alert alert-error">{}</div>"#, msg),
    )
}

#[derive(Deserialize)]
struct SettingsForm {
    bot_token: Option<String>,
    owner_id: Option<String>,
    default_channel: Option<String>,
    crypto_token: Option<String>,
}

/// Trimmed, non-empty view of an optional form field: the single definition
/// of "blank = keep current" for settings saves.
fn non_blank(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

async fn settings_save(
    State(state): State<WebState>,
    headers: axum::http::HeaderMap,
    Form(f): Form<SettingsForm>,
) -> impl IntoResponse {
    let session = match session_from_headers(&state, &headers) {
        Some(s) => s,
        None => return Redirect::to(&state.url("/login")).into_response(),
    };
    if session.role != "owner" {
        return forbidden_page(&state, "Settings", "Settings");
    }
    // Build updates ONLY for non-blank fields: a blank field means "keep the
    // current value", so the secrets already in .env can never be clobbered
    // by an empty form field.
    let mut updates: Vec<(String, String)> = Vec::new();
    if let Some(t) = non_blank(&f.bot_token) {
        // A changed token is validated before anything is written: a typo
        // must never take down the bot's connection on the next restart.
        if !validate_bot_token(t).await {
            return settings_error(
                &state,
                "Invalid bot token (Telegram rejected it). Nothing was saved.",
            );
        }
        updates.push(("TELEGRAM_BOT_TOKEN".to_string(), t.to_string()));
    }
    if let Some(o) = non_blank(&f.owner_id) {
        match o.parse::<i64>() {
            Ok(v) if v > 0 => updates.push(("OWNER_ID".to_string(), v.to_string())),
            _ => {
                return settings_error(
                    &state,
                    "Owner ID must be a positive number. Nothing was saved.",
                );
            }
        }
    }
    if let Some(c) = non_blank(&f.default_channel) {
        updates.push(("DEFAULT_CHANNEL".to_string(), c.to_string()));
    }
    if let Some(c) = non_blank(&f.crypto_token) {
        updates.push(("CRYPTO_PAY_TOKEN".to_string(), c.to_string()));
    }
    if updates.is_empty() {
        // Nothing to change — do not touch .env as a side effect of an
        // all-blank submit.
        return settings_response(
            &state,
            r#"<div class="alert alert-info">Nothing to save — all fields were blank.</div>"#
                .to_string(),
        );
    }
    match crate::config::write_env_file(&updates) {
        Ok(_) => settings_response(
            &state,
            r#"<div class="alert alert-success">Saved. The bot connects on restart.</div>
<p class="help">Values are written to .env; the running process keeps the settings snapshot it started with, so restart to apply them.</p>"#
                .to_string(),
        ),
        Err(e) => settings_error(
            &state,
            &format!("Failed to write .env: {}", esc(&e.to_string())),
        ),
    }
}

#[derive(Deserialize)]
struct SettingsPasswordForm {
    new_password: String,
}

async fn settings_password(
    State(state): State<WebState>,
    headers: axum::http::HeaderMap,
    Form(f): Form<SettingsPasswordForm>,
) -> impl IntoResponse {
    let session = match session_from_headers(&state, &headers) {
        Some(s) => s,
        None => return Redirect::to(&state.url("/login")).into_response(),
    };
    if session.role != "owner" {
        return forbidden_page(&state, "Settings", "Settings");
    }
    // Mirrors the api.rs auth_password contract exactly, no invention: the
    // owner's web session is the strongest identity a deployment has, so no
    // current password is demanded of it; the 6-character floor is the same.
    if f.new_password.chars().count() < 6 {
        return settings_error(&state, "Password must be at least 6 characters.");
    }
    if let Err(e) = state
        .db
        .update_web_password_by_id(session.account_id, &f.new_password)
        .await
    {
        return settings_error(
            &state,
            &format!("Could not change the password: {}", esc(&e)),
        );
    }
    // Rotation evicts every OTHER session of this account (a stolen session
    // must not survive it); the caller keeps theirs — same helper api.rs uses.
    if let Some(tok) = caller_token(&headers) {
        state.db.delete_other_web_sessions(session.account_id, &tok);
    }
    settings_response(
        &state,
        r#"<div class="alert alert-success">Password changed. Other sessions were signed out.</div>"#
            .to_string(),
    )
}

/// The session token behind a request, resolved the way
/// `session_from_headers` does (cookie first, then Bearer) so the password
/// rotation can keep the caller's session while evicting the others.
fn caller_token(headers: &axum::http::HeaderMap) -> Option<String> {
    if let Some(t) = get_cookie(headers) {
        return Some(t);
    }
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t.to_string())
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
        .unwrap_or_else(|| state.db.default_channel().unwrap_or_default());
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
<p class="help">You can skip and configure the bot later in Settings — the console works without it.</p>
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
<div class="form-row">
<div><button type="submit">Save and continue</button></div>
<div><button type="submit" class="btn btn-secondary" formaction="{}" formnovalidate>Skip for now</button></div>
</div>
</form>"#,
        state.url("/setup"),
        state.url("/setup/skip")
    );
    Html(html_page_bare(&state.base_path, "Setup", &body)).into_response()
}

/// "Skip for now": close the first-run gate WITHOUT creating an account or
/// writing .env. The close-on-leave invariant holds — once `setup_done` is
/// set, no page redirects back here, the console simply starts unconfigured
/// (setup mode) and configuration waits in Settings.
async fn setup_skip(State(state): State<WebState>) -> impl IntoResponse {
    if setup_done(&state) {
        return Redirect::to(&state.url("/login")).into_response();
    }
    state.db.set_config("setup_done", "1");
    Redirect::to(&state.url("/login")).into_response()
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

    // Write .env through the shared key-preserving writer (extracted to
    // config.rs, 0600): exactly the keys this form owns are rewritten, every
    // other line (WEB_BIND, WEB_URL, WEB_BASE_PATH, ...) survives. Blank
    // optionals are written as empty values, matching the original writer's
    // behavior so a re-run is idempotent.
    let updates = [
        ("TELEGRAM_BOT_TOKEN".to_string(), token.clone()),
        ("OWNER_ID".to_string(), owner_id.to_string()),
        (
            "DATABASE_URL".to_string(),
            format!("sqlite:{}", state.cfg.db_path),
        ),
        (
            "DEFAULT_CHANNEL".to_string(),
            channel.clone().unwrap_or_default(),
        ),
        (
            "CRYPTO_PAY_TOKEN".to_string(),
            crypto.clone().unwrap_or_default(),
        ),
        ("WEB_USERNAME".to_string(), uname.clone()),
        ("WEB_PASSWORD".to_string(), pw.clone()),
        ("WEB_PORT".to_string(), state.cfg.web_port().to_string()),
        ("RUST_LOG".to_string(), "info".to_string()),
    ];
    if let Err(e) = crate::config::write_env_file(&updates) {
        return setup_error(
            &state,
            &format!("Failed to write .env: {}", esc(&e.to_string())),
        );
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

    /// Serializes tests that touch the process environment (ENV_FILE): env
    /// is global and cargo runs tests in parallel threads, so env-mutating
    /// tests take this lock — held across the awaited request, which is why
    /// it is async-aware (a std guard would poison the runtime's threads).
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Point the shared env writer at a test-owned file. SAFETY: ENV_FILE is
    /// written only while ENV_LOCK is held and no other code in the test
    /// binary reads it concurrently (single-writer mutation).
    fn set_env_file_var(path: &std::path::Path) {
        // SAFETY: serialized by ENV_LOCK.
        unsafe { std::env::set_var("ENV_FILE", path) };
    }

    fn clear_env_file_var() {
        // SAFETY: serialized by ENV_LOCK.
        unsafe { std::env::remove_var("ENV_FILE") };
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

    // ---- Skippable setup + claim + settings + channels (PLAN v1.3.x) ----

    /// A state that has CLOSED setup (setup_done=1) but created no owner —
    /// the post-skip world, where /login is the claim form.
    async fn make_unclaimed_state() -> (WebState, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Db::new(db_path.to_str().unwrap()).unwrap();
        db.set_config("setup_done", "1");
        let cfg = Config {
            bot_token: String::new(),
            owner_id: 0,
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
        let state = WebState {
            db: Arc::new(db),
            cfg,
            bot: teloxide::Bot::new("0:dummy"),
            login_attempts: Arc::new(StdMutex::new(HashMap::new())),
            base_path: String::new(),
        };
        (state, dir)
    }

    fn get_req(uri: &str) -> Request<Body> {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    fn post_req(uri: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn authed_req(uri: &str, token: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header("cookie", format!("session={}", token))
            .body(Body::empty())
            .unwrap()
    }

    fn post_authed_req(uri: &str, body: &str, token: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("cookie", format!("session={}", token))
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    /// A loopback or remote peer, injected the way
    /// `into_make_service_with_connect_info` would (as the ConnectInfo
    /// extension, which the PeerIp extractor reads).
    fn with_peer(req: Request<Body>, ip: [u8; 4]) -> Request<Body> {
        let (mut parts, body) = req.into_parts();
        parts
            .extensions
            .insert(ConnectInfo(SocketAddr::new(IpAddr::from(ip), 40000)));
        Request::from_parts(parts, body)
    }

    async fn owner_session(state: &WebState) -> String {
        let (acc_id, _u, _h, _r, _s) = state
            .db
            .get_web_account_by_username("owner")
            .expect("owner account exists");
        state.db.create_web_session(acc_id, "owner", None, 7)
    }

    async fn seller_session(state: &WebState, seller_id: i64) -> String {
        state
            .db
            .create_web_account("seller", "secret", "seller", Some(seller_id))
            .await
            .unwrap();
        let (acc_id, _u, _h, _r, _s) = state
            .db
            .get_web_account_by_username("seller")
            .expect("seller account exists");
        state
            .db
            .create_web_session(acc_id, "seller", Some(seller_id), 7)
    }

    #[tokio::test]
    async fn skip_sets_setup_done_and_creates_no_account() {
        let (state, _dir) = make_unclaimed_state().await;
        let db = state.db.clone();
        let app = router(state);
        let resp = app.oneshot(post_req("/setup/skip", "")).await.unwrap();
        assert!(resp.status().is_redirection());
        assert_eq!(
            resp.headers().get("location").unwrap(),
            "/login",
            "skip lands on the (claim) front door"
        );
        // The B2 close-on-leave invariant: the gate is closed for good...
        assert_eq!(db.get_config("setup_done").as_deref(), Some("1"));
        // ...but nothing else happened: no account, no configured snapshot.
        assert!(db.get_web_account_by_username("owner").is_none());
        assert!(!db.owner_account_exists());
    }

    #[tokio::test]
    async fn skip_after_done_redirects_to_login() {
        let (state, _dir) = make_unclaimed_state().await;
        state.db.set_config("setup_done", "1");
        let app = router(state);
        let resp = app.oneshot(post_req("/setup/skip", "")).await.unwrap();
        assert!(resp.status().is_redirection());
        assert_eq!(resp.headers().get("location").unwrap(), "/login");
    }

    #[tokio::test]
    async fn claim_page_renders_when_no_owner() {
        let (state, _dir) = make_unclaimed_state().await;
        let app = router(state);
        let resp = app.oneshot(get_req("/login")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;
        assert!(body.contains("Claim the console"), "{body}");
        assert!(
            body.contains(r#"autocomplete="new-password""#),
            "claim form asks for a NEW password:\n{body}"
        );
    }

    #[tokio::test]
    async fn claim_blocked_off_loopback_without_code() {
        let (state, _dir) = make_unclaimed_state().await;
        let db = state.db.clone();
        // A live code exists on the server; the attacker does not have it.
        db.set_config("claim_code", "code-12345");
        // Remote peer, no code: refused, nothing created, no claim hint leak.
        let req = with_peer(
            post_req(
                "/login",
                "username=admin&password=hunter2&confirm_password=hunter2",
            ),
            [203, 0, 113, 7],
        );
        let resp = router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;
        assert!(
            body.contains("Claiming requires the one-time code shown in the server console."),
            "{body}"
        );
        assert!(!body.contains("hunter2"), "form input echoed back: {body}");
        assert!(
            !body.contains("code-12345"),
            "the server's claim code must never be echoed: {body}"
        );
        assert!(!db.owner_account_exists());

        // A wrong code from a remote peer: same refusal, still no echo.
        let req = with_peer(
            post_req(
                "/login",
                "username=admin&password=hunter2&confirm_password=hunter2&claim_code=wrong",
            ),
            [203, 0, 113, 7],
        );
        let resp = router(state).oneshot(req).await.unwrap();
        let body = body_string(resp).await;
        assert!(
            body.contains("Claiming requires the one-time code"),
            "{body}"
        );
        assert!(
            !body.contains("code-12345"),
            "real code must not appear when a wrong one is tried: {body}"
        );
        assert!(!db.owner_account_exists());
    }

    #[tokio::test]
    async fn claim_with_code_works_and_creates_owner_session() {
        let (state, _dir) = make_unclaimed_state().await;
        // The claim code is generated at startup and printed to the console;
        // tests seed it the same place production stores it.
        state.db.set_config("claim_code", "code-12345");
        let db = state.db.clone();
        let app = router(state);
        // No peer extension (unknown, i.e. remote) — the code alone must be
        // sufficient.
        let resp = app
            .oneshot(post_req(
                "/login",
                "username=root&password=secret1&confirm_password=secret1&claim_code=code-12345",
            ))
            .await
            .unwrap();
        assert!(resp.status().is_redirection());
        assert_eq!(resp.headers().get("location").unwrap(), "/dashboard");
        let cookie = resp
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(cookie.contains("session="), "{cookie}");
        let (acc_id, _u, _h, role, seller_id) =
            db.get_web_account_by_username("root").expect("claimed");
        assert_eq!(role, "owner");
        assert_eq!(seller_id, None);
        // The presented session works and belongs to the new owner account.
        let token = cookie
            .split(';')
            .next()
            .unwrap()
            .trim_start_matches("session=");
        assert_eq!(
            db.get_web_session(token),
            Some((acc_id, "owner".to_string(), None))
        );
    }

    #[tokio::test]
    async fn claim_from_loopback_needs_no_code() {
        let (state, _dir) = make_unclaimed_state().await;
        let db = state.db.clone();
        let app = router(state);
        let req = with_peer(
            post_req(
                "/login",
                "username=root&password=secret1&confirm_password=secret1",
            ),
            [127, 0, 0, 1],
        );
        let resp = app.oneshot(req).await.unwrap();
        assert!(resp.status().is_redirection(), "loopback claims freely");
        assert_eq!(resp.headers().get("location").unwrap(), "/dashboard");
        assert!(db.owner_account_exists());
    }

    #[tokio::test]
    async fn claim_mismatched_confirmation_refused() {
        let (state, _dir) = make_unclaimed_state().await;
        let db = state.db.clone();
        let app = router(state);
        let req = with_peer(
            post_req(
                "/login",
                "username=root&password=secret1&confirm_password=secret2",
            ),
            [127, 0, 0, 1],
        );
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;
        assert!(body.contains("Passwords do not match"), "{body}");
        assert!(!db.owner_account_exists());
    }

    #[tokio::test]
    async fn claim_impossible_once_owner_exists() {
        let (state, _dir) = make_state().await;
        let db = state.db.clone();
        let app = router(state);
        // GET: the front door is the sign-in form again — claim mode never
        // returns.
        let resp = app.clone().oneshot(get_req("/login")).await.unwrap();
        let body = body_string(resp).await;
        assert!(body.contains("Sign in"), "{body}");
        assert!(!body.contains("Claim the console"), "{body}");
        // POST with claim-shaped fields: treated as a plain login attempt.
        let resp = app
            .oneshot(post_req(
                "/login",
                "username=root&password=secret1&confirm_password=secret1&claim_code=code-12345",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;
        assert!(body.contains("Invalid credentials"), "{body}");
        assert!(db.get_web_account_by_username("root").is_none());
        assert!(db.owner_account_exists());
    }

    #[tokio::test]
    async fn settings_seller_refused() {
        let (state, _dir) = make_state().await;
        let token = seller_session(&state, 42).await;
        let app = router(state);
        for uri in ["/settings"] {
            let resp = app.clone().oneshot(authed_req(uri, &token)).await.unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{uri}");
            assert!(
                body_string(resp)
                    .await
                    .contains("restricted to the console owner")
            );
        }
        for (uri, body) in [
            ("/settings/save", "owner_id=42"),
            ("/settings/password", "new_password=secret1"),
        ] {
            let resp = app
                .clone()
                .oneshot(post_authed_req(uri, body, &token))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{uri}");
        }
    }

    #[tokio::test]
    async fn settings_save_blank_fields_change_nothing() {
        let (state, dir) = make_state().await;
        let token = owner_session(&state).await;
        let app = router(state);
        let _env = ENV_LOCK.lock().await;
        let env_path = dir.path().join(".env");
        set_env_file_var(&env_path);
        let resp = app
            .oneshot(post_authed_req(
                "/settings/save",
                "bot_token=&owner_id=&default_channel=&crypto_token=",
                &token,
            ))
            .await
            .unwrap();
        let body = body_string(resp).await;
        assert!(body.contains("Nothing to save"), "{body}");
        assert!(!env_path.exists(), "all-blank save must not create .env");
        clear_env_file_var();
    }

    #[tokio::test]
    async fn settings_save_writes_env_via_helper() {
        let (state, dir) = make_state().await;
        let token = owner_session(&state).await;
        let app = router(state);
        let _env = ENV_LOCK.lock().await;
        let env_path = dir.path().join(".env");
        set_env_file_var(&env_path);
        // No bot token in the form (that path validates against Telegram);
        // the other fields go straight through the shared writer.
        let resp = app
            .oneshot(post_authed_req(
                "/settings/save",
                "owner_id=42&default_channel=@shop",
                &token,
            ))
            .await
            .unwrap();
        let body = body_string(resp).await;
        assert!(
            body.contains("Saved. The bot connects on restart."),
            "{body}"
        );
        let env = std::fs::read_to_string(&env_path).expect(".env written");
        assert!(env.contains("OWNER_ID=42"), "{env}");
        assert!(env.contains("DEFAULT_CHANNEL=@shop"), "{env}");
        clear_env_file_var();
    }

    #[tokio::test]
    async fn settings_save_rejects_invalid_token_without_writing() {
        let (state, dir) = make_state().await;
        let token = owner_session(&state).await;
        let app = router(state);
        let _env = ENV_LOCK.lock().await;
        let env_path = dir.path().join(".env");
        set_env_file_var(&env_path);
        let resp = app
            .oneshot(post_authed_req(
                "/settings/save",
                "bot_token=123456:definitely-not-a-token",
                &token,
            ))
            .await
            .unwrap();
        let body = body_string(resp).await;
        assert!(body.contains("Invalid bot token"), "{body}");
        assert!(!env_path.exists(), "a rejected token must never reach .env");
        clear_env_file_var();
    }

    #[tokio::test]
    async fn settings_password_changes_and_invalidates_other_sessions() {
        let (state, _dir) = make_state().await;
        let db = state.db.clone();
        let kept = owner_session(&state).await;
        let (acc_id, _u, _h, _r, _s) = db.get_web_account_by_username("owner").unwrap();
        let other = db.create_web_session(acc_id, "owner", None, 7);
        let app = router(state);
        let resp = app
            .oneshot(post_authed_req(
                "/settings/password",
                "new_password=fresh1",
                &kept,
            ))
            .await
            .unwrap();
        let body = body_string(resp).await;
        assert!(
            body.contains("Password changed. Other sessions were signed out."),
            "{body}"
        );
        // The caller's session survives; every other session of the account
        // is gone; the new password verifies.
        assert!(db.get_web_session(&kept).is_some());
        assert!(db.get_web_session(&other).is_none());
        assert!(db.verify_web_password("owner", "fresh1").await.is_some());
    }

    #[tokio::test]
    async fn channels_add_default_remove_scoping() {
        let (state, _dir) = make_state().await;
        let db = state.db.clone();
        let owner = owner_session(&state).await;
        let seller = seller_session(&state, 42).await;
        let app = router(state);

        // Owner adds to the global set (seller_id 0).
        let resp = app
            .clone()
            .oneshot(post_authed_req("/channels/add", "channel=@one", &owner))
            .await
            .unwrap();
        assert!(resp.status().is_redirection());
        let resp = app
            .clone()
            .oneshot(post_authed_req("/channels/add", "channel=@two", &owner))
            .await
            .unwrap();
        assert!(resp.status().is_redirection());

        // Set default; the page shows the badge and hides Remove on that row.
        let resp = app
            .clone()
            .oneshot(post_authed_req("/channels/default", "channel=@one", &owner))
            .await
            .unwrap();
        assert!(resp.status().is_redirection());
        assert_eq!(db.default_channel().as_deref(), Some("@one"));
        let resp = app
            .clone()
            .oneshot(authed_req("/channels", &owner))
            .await
            .unwrap();
        let body = body_string(resp).await;
        assert!(body.contains("@one") && body.contains("@two"), "{body}");
        assert!(body.contains("default"), "{body}");

        // Removing the default is refused, inline, and removes nothing.
        let resp = app
            .clone()
            .oneshot(post_authed_req("/channels/remove", "channel=@one", &owner))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "inline error, not PRG");
        let body = body_string(resp).await;
        assert!(body.contains("cannot be removed"), "{body}");
        assert_eq!(db.default_channel().as_deref(), Some("@one"));

        // A non-default row can take over, after which the old default can
        // be removed.
        let resp = app
            .clone()
            .oneshot(post_authed_req("/channels/default", "channel=@two", &owner))
            .await
            .unwrap();
        assert!(resp.status().is_redirection());
        let resp = app
            .clone()
            .oneshot(post_authed_req("/channels/remove", "channel=@one", &owner))
            .await
            .unwrap();
        assert!(resp.status().is_redirection());
        assert_eq!(db.default_channel().as_deref(), Some("@two"));

        // Seller scope: adding sets the seller's own channel, removing
        // touches only their own row, and the default endpoint is refused.
        let resp = app
            .clone()
            .oneshot(post_authed_req("/channels/add", "channel=@mine", &seller))
            .await
            .unwrap();
        assert!(resp.status().is_redirection());
        assert_eq!(db.seller_channel(42).as_deref(), Some("@mine"));
        // The seller's panel shows their bot-managed channel read-only.
        let resp = app
            .clone()
            .oneshot(authed_req("/channels", &seller))
            .await
            .unwrap();
        let body = body_string(resp).await;
        assert!(body.contains("@mine"), "{body}");
        assert!(body.contains("bot-managed"), "{body}");
        assert!(
            !body.contains("Global channels"),
            "seller must not see the owner's panel: {body}"
        );
        let resp = app
            .clone()
            .oneshot(post_authed_req("/channels/remove", "channel=@two", &seller))
            .await
            .unwrap();
        // The scoped writer refuses a cross-scope remove honestly: the page
        // re-renders with the refusal and the global row survives.
        assert_eq!(resp.status(), StatusCode::OK, "inline error, not PRG");
        let body = body_string(resp).await;
        assert!(body.contains("not yours to remove"), "{body}");
        assert!(
            db.default_channel().as_deref() == Some("@two"),
            "seller remove must stay scoped to their own rows"
        );
        let resp = app
            .clone()
            .oneshot(post_authed_req(
                "/channels/default",
                "channel=@mine",
                &seller,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn post_form_renders_channel_dropdown() {
        let (state, _dir) = make_state().await;
        state.db.add_channel("@one", 0).unwrap();
        state.db.add_channel("@two", 0).unwrap();
        state.db.set_seller_channel(42, "@mine");
        let owner = owner_session(&state).await;
        let seller = seller_session(&state, 42).await;
        let app = router(state);
        // Owner scope (None = every table row): (default) first, then the
        // channels. The seller's bot-managed channel is NOT here — it lives
        // in the legacy sellers column, not the channels table.
        let resp = app
            .clone()
            .oneshot(authed_req("/dashboard", &owner))
            .await
            .unwrap();
        let body = body_string(resp).await;
        assert!(body.contains(r#"<select name="channel">"#), "{body}");
        assert!(
            body.contains(r#"<option value="">(default)</option>"#),
            "{body}"
        );
        assert!(
            body.contains(r#"<option value="@one">@one</option>"#),
            "{body}"
        );
        assert!(
            !body.contains(r#"<option value="@mine">"#),
            "legacy seller channel must not leak into the owner's dropdown: {body}"
        );
        // Configured console shows no connect banner.
        assert!(
            !body.contains("The bot is not connected yet"),
            "configured state must not warn: {body}"
        );

        // Seller scope = the global namespace plus their own rows (the
        // landed list_channels contract), plus their merged legacy channel,
        // plus the (default) option.
        let resp = app
            .oneshot(authed_req("/dashboard", &seller))
            .await
            .unwrap();
        let body = body_string(resp).await;
        assert!(
            body.contains(r#"<option value="@mine">@mine</option>"#),
            "{body}"
        );
        assert!(
            body.contains(r#"<option value="@one">@one</option>"#),
            "{body}"
        );
    }

    #[tokio::test]
    async fn dashboard_shows_connect_banner_when_unconfigured() {
        let (state, _dir) = make_unclaimed_state().await;
        // A claimed-but-never-configured console: owner account exists, the
        // bot token never landed.
        state
            .db
            .create_web_account("root", "secret1", "owner", None)
            .await
            .unwrap();
        let (acc_id, _u, _h, _r, _s) = state.db.get_web_account_by_username("root").unwrap();
        let token = state.db.create_web_session(acc_id, "owner", None, 7);
        let app = router(state);
        let resp = app.oneshot(authed_req("/dashboard", &token)).await.unwrap();
        let body = body_string(resp).await;
        assert!(body.contains("The bot is not connected yet."), "{body}");
        assert!(
            body.contains(r#"href="/settings""#),
            "banner links to Settings: {body}"
        );
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
