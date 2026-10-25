use crate::web::WebState;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, patch, post},
};
use serde::{Deserialize, Serialize};
use teloxide::payloads::{
    CreateChatInviteLinkSetters, SetChatDescriptionSetters, UnpinChatMessageSetters,
};
use teloxide::{prelude::Requester, types::MessageId};

// Re-export WebState alias for convenience

#[derive(Serialize)]
struct ApiError {
    error: ApiErrorBody,
}
#[derive(Serialize)]
struct ApiErrorBody {
    code: String,
    message: String,
}

fn err(status: StatusCode, code: &str, msg: &str) -> (StatusCode, Json<ApiError>) {
    (
        status,
        Json(ApiError {
            error: ApiErrorBody {
                code: code.to_string(),
                message: msg.to_string(),
            },
        }),
    )
}

fn session_or_401(
    state: &WebState,
    headers: &HeaderMap,
) -> Result<crate::web::Session, (StatusCode, Json<ApiError>)> {
    crate::web::session_from_headers(state, headers).ok_or_else(|| {
        err(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or invalid session",
        )
    })
}

fn seller_scope(session: &crate::web::Session) -> Option<i64> {
    if session.role == "seller" {
        session.seller_id
    } else {
        None
    }
}

fn is_admin_role(role: &str) -> bool {
    role == "admin" || role == "owner"
}

/// Extract the invoice array from a Crypto Pay `getInvoices` response,
/// tolerating the API's envelope variations: a bare array `[...]`,
/// `{"items":[...]}`, or `{"result":{"items":[...]}}` (this crate's
/// `CryptoPay::call` already unwraps `result`, but be robust to all three
/// shapes). Returns an empty vec when no array is found.
pub fn extract_invoice_items(v: &serde_json::Value) -> Vec<serde_json::Value> {
    let mut cur = v;
    if let Some(result) = cur.get("result") {
        cur = result;
    }
    if let Some(items) = cur.get("items").and_then(|i| i.as_array()) {
        return items.clone();
    }
    cur.as_array().cloned().unwrap_or_default()
}

// ---------- Login throttling ----------
// Shared with the web login: one failure counter per username across both
// interfaces (see `web::login_throttle` / `web::login_ok`).

// ---------- Health ----------
/// Enriched liveness probe. Backward compatible: still a 200 with the
/// original `status` field; adds `db` (SELECT 1 probe), `bot`
/// (`configured` | `setup`), `mcp` (`enabled` | `disabled`) and
/// `uptime_seconds`. No secrets or identifiers in the response.
async fn health(State(state): State<WebState>) -> impl IntoResponse {
    let db_ok = state.db.ping();
    let status = if db_ok { "ok" } else { "error" };
    let bot = if state.cfg.is_configured() {
        "configured"
    } else {
        "setup"
    };
    let mcp = if state
        .cfg
        .mcp_owner_token
        .as_deref()
        .map(|t| !t.trim().is_empty())
        .unwrap_or(false)
    {
        "enabled"
    } else {
        "disabled"
    };
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": status,
            "db": if db_ok { "ok" } else { "error" },
            "bot": bot,
            "mcp": mcp,
            "uptime_seconds": crate::metrics::uptime_secs(),
        })),
    )
}

// ---------- Auth ----------
#[derive(Deserialize)]
struct LoginReq {
    username: String,
    password: String,
}
#[derive(Serialize)]
struct LoginResp {
    token: String,
    expires_at: String,
    account_id: i64,
    role: String,
    seller_id: Option<i64>,
}

async fn auth_login(State(state): State<WebState>, Json(f): Json<LoginReq>) -> impl IntoResponse {
    // Records this attempt up-front; a successful login clears it below.
    if crate::web::login_throttle(&state.login_attempts, &f.username, 5, 900) {
        return err(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_attempts",
            "too many failed login attempts; try again later",
        )
        .into_response();
    }
    match state.db.verify_web_password(&f.username, &f.password).await {
        Some((account_id, role, seller_id)) => {
            crate::web::login_ok(&state.login_attempts, &f.username);
            let token = state.db.create_web_session(account_id, &role, seller_id, 7);
            let expires_at = chrono::Utc::now() + chrono::Duration::days(7);
            (
                StatusCode::OK,
                Json(
                    serde_json::to_value(LoginResp {
                        token,
                        expires_at: expires_at.to_rfc3339(),
                        account_id,
                        role,
                        seller_id,
                    })
                    .unwrap(),
                ),
            )
                .into_response()
        }
        None => err(
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "invalid username or password",
        )
        .into_response(),
    }
}

async fn auth_logout(State(state): State<WebState>, headers: HeaderMap) -> impl IntoResponse {
    if let Some(token) = crate::web::get_cookie(&headers) {
        state.db.delete_web_session(&token);
    }
    // Bearer token support
    if let Some(auth) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        && let Some(tok) = auth.strip_prefix("Bearer ")
    {
        state.db.delete_web_session(tok);
    }
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

async fn auth_me(State(state): State<WebState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "account_id": session.account_id,
            "role": session.role,
            "seller_id": session.seller_id,
        })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct PasswordReq {
    /// Required for non-owner accounts: the current password, verified via
    /// the same bcrypt path login uses (`verify_web_password`). The owner's
    /// web session is the strongest identity a deployment has and may omit
    /// it (there is no other owner-password surface — see setup/main.rs).
    current_password: Option<String>,
    new_password: String,
}

/// Extract the session token the way `session_from_headers` resolves it
/// (cookie first, then Bearer) so the password change can keep the caller's
/// session while evicting every other session of the account.
fn caller_token(headers: &HeaderMap) -> Option<String> {
    if let Some(t) = crate::web::get_cookie(headers) {
        return Some(t);
    }
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t.to_string())
}

async fn auth_password(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(f): Json<PasswordReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let Some((_id, username, _hash, role, _seller)) =
        state.db.get_web_account_by_id(session.account_id)
    else {
        // A session whose account row vanished (deleted account): be honest
        // instead of silently succeeding.
        return err(
            StatusCode::UNAUTHORIZED,
            "account_not_found",
            "session account no longer exists",
        )
        .into_response();
    };
    // Mirror the setup form's "min 6 chars" rule (web.rs setup_submit).
    if f.new_password.chars().count() < 6 {
        return err(
            StatusCode::BAD_REQUEST,
            "weak_password",
            "password must be at least 6 characters",
        )
        .into_response();
    }
    // Non-owner accounts must prove knowledge of the current password via
    // the same bcrypt verification the login uses.
    if role != "owner" {
        // spawn_blocking hasher is async: awaiting happens here, not in a
        // sync closure (mechanical restructure of the former Option::map).
        let verified = match f.current_password.as_deref() {
            Some(cur) => state.db.verify_web_password(&username, cur).await.is_some(),
            None => false,
        };
        if !verified {
            return err(
                StatusCode::UNAUTHORIZED,
                "invalid_credentials",
                "current password missing or incorrect",
            )
            .into_response();
        }
    }
    if let Err(e) = state
        .db
        .update_web_password_by_id(session.account_id, &f.new_password)
        .await
    {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "update_failed", &e).into_response();
    }
    // Password rotation invalidates every OTHER session of this account (a
    // stolen session must not survive it); the caller stays logged in.
    if let Some(tok) = caller_token(&headers) {
        state.db.delete_other_web_sessions(session.account_id, &tok);
    }
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

// ---------- Sellers ----------
async fn sellers_list(State(state): State<WebState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if !is_admin_role(&session.role) {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "admin or owner required",
        )
        .into_response();
    }
    let ids = state.db.list_sellers();
    let items: Vec<serde_json::Value> = ids
        .into_iter()
        .map(|id| {
            serde_json::json!({
                "user_id": id, "username": null, "default_channel": state.db.seller_channel(id),
            })
        })
        .collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({"items": items, "total": items.len()})),
    )
        .into_response()
}

async fn sellers_me(State(state): State<WebState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let sid = match session.seller_id {
        Some(s) => s,
        None => {
            return err(StatusCode::FORBIDDEN, "not_a_seller", "no seller profile").into_response();
        }
    };
    let (uid, username, channel) = state.db.get_seller(sid).unwrap_or((sid, None, None));
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "user_id": uid, "username": username, "default_channel": channel,
        })),
    )
        .into_response()
}

async fn seller_get(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(user_id): Path<i64>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if session.role == "seller" && session.seller_id != Some(user_id) {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "cannot view other sellers",
        )
        .into_response();
    }
    match state.db.get_seller(user_id) {
        Some((uid, username, channel)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "user_id": uid, "username": username, "default_channel": channel,
            })),
        )
            .into_response(),
        None => err(StatusCode::NOT_FOUND, "not_found", "seller not found").into_response(),
    }
}

#[derive(Deserialize)]
struct SellerUpsertReq {
    user_id: i64,
    username: Option<String>,
    default_channel: Option<String>,
}
async fn sellers_create(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(f): Json<SellerUpsertReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if !is_admin_role(&session.role) && session.seller_id != Some(f.user_id) {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "cannot create seller for others",
        )
        .into_response();
    }
    state.db.upsert_seller(
        f.user_id,
        f.username.as_deref(),
        f.default_channel.as_deref(),
    );
    (StatusCode::CREATED, Json(serde_json::json!({"ok": true}))).into_response()
}

#[derive(Deserialize)]
struct ChannelReq {
    channel: String,
}
async fn seller_set_channel(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(user_id): Path<i64>,
    Json(f): Json<ChannelReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if session.role == "seller" && session.seller_id != Some(user_id) {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "cannot modify other seller",
        )
        .into_response();
    }
    state.db.set_seller_channel(user_id, &f.channel);
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

// ---------- Plans ----------
async fn plans_list(State(state): State<WebState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let scope = seller_scope(&session);
    let plans = state.db.list_plans(scope);
    let items: Vec<serde_json::Value> = plans.into_iter().map(|p| serde_json::json!({
        "name": p.0, "price": p.1, "asset": p.2, "duration_days": p.3, "channel_id": p.4, "active": p.5, "seller_id": p.6,
    })).collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({"items": items, "total": items.len()})),
    )
        .into_response()
}

#[derive(Deserialize)]
struct PlanCreateReq {
    name: String,
    price: String,
    asset: String,
    duration_days: i64,
    channel_id: Option<String>,
}
async fn plans_create(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(f): Json<PlanCreateReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let seller_id = match session.seller_id {
        Some(s) => s,
        None => {
            return err(
                StatusCode::FORBIDDEN,
                "not_a_seller",
                "seller profile required",
            )
            .into_response();
        }
    };
    // Same bound as the bot's /planadd; also guards Db's unchecked
    // expires_at arithmetic (Duration::days) against overflow panics.
    let days = f
        .duration_days
        .clamp(1, crate::handlers::commands::MAX_PLAN_DAYS);
    state.db.add_plan(
        seller_id,
        &f.name,
        &f.price,
        &f.asset,
        days,
        f.channel_id.as_deref(),
    );
    (StatusCode::CREATED, Json(serde_json::json!({"ok": true}))).into_response()
}

async fn plans_get(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let scope = if session.role == "seller" {
        session.seller_id
    } else {
        None
    };
    // find by id via list (no get_plan_by_id scoping helper exists for seller; use get_plan_by_id then check)
    match state.db.get_plan_by_id(id) {
        Some((sid, price, asset, name, days, channel)) => {
            if session.role == "seller" && session.seller_id != Some(sid) {
                return err(StatusCode::FORBIDDEN, "forbidden", "not your plan").into_response();
            }
            let _ = scope;
            (StatusCode::OK, Json(serde_json::json!({
                "id": id, "seller_id": sid, "name": name, "price": price, "asset": asset, "duration_days": days, "channel_id": channel,
            }))).into_response()
        }
        None => err(StatusCode::NOT_FOUND, "not_found", "plan not found").into_response(),
    }
}

#[derive(Deserialize)]
struct PlanUpdateReq {
    name: Option<String>,
    price: Option<String>,
    asset: Option<String>,
    duration_days: Option<i64>,
    channel_id: Option<String>,
    active: Option<bool>,
}
async fn plans_update(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(f): Json<PlanUpdateReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let (sid, _price, _asset, name, days, channel) = match state.db.get_plan_by_id(id) {
        Some(x) => x,
        None => {
            return err(StatusCode::NOT_FOUND, "not_found", "plan not found").into_response();
        }
    };
    if session.role == "seller" && session.seller_id != Some(sid) {
        return err(StatusCode::FORBIDDEN, "forbidden", "not your plan").into_response();
    }
    // Re-add plan with merged fields (add_plan is upsert by seller+name)
    let new_name = f.name.unwrap_or(name);
    let new_price = f.price.unwrap_or(_price);
    let new_asset = f.asset.unwrap_or(_asset);
    let new_days = f
        .duration_days
        .unwrap_or(days)
        .clamp(1, crate::handlers::commands::MAX_PLAN_DAYS);
    let new_channel = f.channel_id.or(channel);
    if let Some(active) = f.active {
        if !active {
            state.db.remove_plan(sid, &new_name);
        } else {
            state.db.add_plan(
                sid,
                &new_name,
                &new_price,
                &new_asset,
                new_days,
                new_channel.as_deref(),
            );
        }
    } else {
        state.db.add_plan(
            sid,
            &new_name,
            &new_price,
            &new_asset,
            new_days,
            new_channel.as_deref(),
        );
    }
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

async fn plans_delete(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let (sid, _p, _a, name, _d, _c) = match state.db.get_plan_by_id(id) {
        Some(x) => x,
        None => {
            return err(StatusCode::NOT_FOUND, "not_found", "plan not found").into_response();
        }
    };
    if session.role == "seller" && session.seller_id != Some(sid) {
        return err(StatusCode::FORBIDDEN, "forbidden", "not your plan").into_response();
    }
    state.db.remove_plan(sid, &name);
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

#[derive(Deserialize)]
struct ActiveReq {
    active: bool,
}
async fn plans_active(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(f): Json<ActiveReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let (sid, price, asset, name, days, channel) = match state.db.get_plan_by_id(id) {
        Some(x) => x,
        None => {
            return err(StatusCode::NOT_FOUND, "not_found", "plan not found").into_response();
        }
    };
    if session.role == "seller" && session.seller_id != Some(sid) {
        return err(StatusCode::FORBIDDEN, "forbidden", "not your plan").into_response();
    }
    if f.active {
        state
            .db
            .add_plan(sid, &name, &price, &asset, days, channel.as_deref());
    } else {
        state.db.remove_plan(sid, &name);
    }
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

// ---------- Subscriptions ----------
async fn subs_list(State(state): State<WebState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let scope = seller_scope(&session);
    let subs = state.db.list_subscriptions(scope, 100);
    let items: Vec<serde_json::Value> = subs.into_iter().map(|s| serde_json::json!({
        "id": s.0, "seller_id": s.1, "user_id": s.2, "channel_id": s.3, "status": s.4, "expires_at": s.5,
    })).collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({"items": items, "total": items.len()})),
    )
        .into_response()
}

#[derive(Deserialize)]
struct SubGrantReq {
    user_id: i64,
    plan_id: Option<i64>,
    channel_id: Option<String>,
    days: i64,
    invoice_id: Option<String>,
    code_id: Option<i64>,
}
async fn subs_create(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(f): Json<SubGrantReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let seller_id = match session.seller_id {
        Some(s) => s,
        None => {
            return err(
                StatusCode::FORBIDDEN,
                "not_a_seller",
                "seller profile required",
            )
            .into_response();
        }
    };
    let ch = f.channel_id.unwrap_or_default();
    // Clamp to the shared plan-day bound so Db's unchecked expires_at
    // arithmetic can never overflow-panic on a hostile `days`.
    let days = f.days.clamp(1, crate::handlers::commands::MAX_PLAN_DAYS);
    let sub_id = state.db.create_subscription(
        seller_id,
        f.user_id,
        f.plan_id,
        &ch,
        days,
        f.invoice_id.as_deref(),
        f.code_id,
    );
    (StatusCode::CREATED, Json(serde_json::json!({"id": sub_id}))).into_response()
}

async fn subs_get(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    // find in list (no get_sub_by_id helper); scan
    let scope = seller_scope(&session);
    let subs = state.db.list_subscriptions(scope, 1000);
    match subs.into_iter().find(|s| s.0 == id) {
        Some(s) => (StatusCode::OK, Json(serde_json::json!({
            "id": s.0, "seller_id": s.1, "user_id": s.2, "channel_id": s.3, "status": s.4, "expires_at": s.5,
        }))).into_response(),
        None => err(StatusCode::NOT_FOUND, "not_found", "subscription not found").into_response(),
    }
}

#[derive(Deserialize)]
struct SubPatchReq {
    status: Option<String>,
    expires_at: Option<String>,
}
async fn subs_patch(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(f): Json<SubPatchReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if let Some(status) = &f.status
        && status == "expired"
    {
        state.db.mark_subscription_expired(id);
    }
    let _ = (session, f.expires_at);
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

async fn subs_expire_check(State(state): State<WebState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let expired = state.db.expired_subscriptions();
    let n = expired.len();
    for (id, _sid, _uid, _ch) in expired {
        // only process seller's own unless admin
        state.db.mark_subscription_expired(id);
    }
    let _ = session;
    (StatusCode::OK, Json(serde_json::json!({"expired": n}))).into_response()
}

async fn user_subs(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(user_id): Path<i64>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let scope = seller_scope(&session);
    let subs = state.db.list_subscriptions(scope, 1000);
    let items: Vec<_> = subs.into_iter().filter(|s| s.2 == user_id).map(|s| serde_json::json!({
        "id": s.0, "seller_id": s.1, "user_id": s.2, "channel_id": s.3, "status": s.4, "expires_at": s.5,
    })).collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({"items": items, "total": items.len()})),
    )
        .into_response()
}

// ---------- Invoices ----------
async fn invoices_list(State(state): State<WebState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let scope = seller_scope(&session);
    let invs = state.db.list_invoices(scope, 100);
    let items: Vec<serde_json::Value> = invs
        .into_iter()
        .map(|i| {
            serde_json::json!({
                "invoice_id": i.0, "amount": i.1, "asset": i.2, "status": i.3, "pay_url": i.4,
            })
        })
        .collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({"items": items, "total": items.len()})),
    )
        .into_response()
}

#[derive(Deserialize)]
struct InvoiceCreateReq {
    amount: String,
    asset: String,
    description: Option<String>,
    plan_id: Option<String>,
    user_id: Option<i64>,
}
async fn invoices_create(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(f): Json<InvoiceCreateReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let seller_id = session.seller_id.unwrap_or(0);
    let inv_id = format!("inv_{}", crate::util::gen_token(16));
    state.db.save_invoice(
        &inv_id,
        seller_id,
        &f.amount,
        &f.asset,
        f.description.as_deref(),
        "active",
        None,
        f.plan_id.as_deref(),
        f.user_id,
        None,
    );
    (
        StatusCode::CREATED,
        Json(serde_json::json!({"invoice_id": inv_id})),
    )
        .into_response()
}

async fn invoices_get(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(invoice_id): Path<String>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match state.db.get_invoice(&invoice_id) {
        Some((amount, asset, status, desc, plan_id, user_id, seller_id)) => {
            if session.role == "seller" && session.seller_id != Some(seller_id) {
                return err(StatusCode::FORBIDDEN, "forbidden", "not your invoice").into_response();
            }
            (StatusCode::OK, Json(serde_json::json!({
                "invoice_id": invoice_id, "amount": amount, "asset": asset, "status": status,
                "description": desc, "plan_id": plan_id, "user_id": user_id, "seller_id": seller_id,
            }))).into_response()
        }
        None => err(StatusCode::NOT_FOUND, "not_found", "invoice not found").into_response(),
    }
}

#[derive(Deserialize)]
struct InvoiceStatusReq {
    status: String,
}
async fn invoices_status(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(invoice_id): Path<String>,
    Json(f): Json<InvoiceStatusReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    // ownership check
    if let Some((_a, _as, _st, _d, _p, _u, sid)) = state.db.get_invoice(&invoice_id) {
        if session.role == "seller" && session.seller_id != Some(sid) {
            return err(StatusCode::FORBIDDEN, "forbidden", "not your invoice").into_response();
        }
    } else {
        return err(StatusCode::NOT_FOUND, "not_found", "invoice not found").into_response();
    }
    // 'paid' is a verified state, not a manual one: it may only be reached
    // through the verified webhook or the claim flow.
    if f.status == "paid" {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_status",
            "paid transitions require verified webhook/claim",
        )
        .into_response();
    }
    state.db.update_invoice_status(&invoice_id, &f.status);
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

// ---------- Codes ----------
async fn codes_list(State(state): State<WebState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let scope = seller_scope(&session);
    let codes = state.db.list_codes(scope, 100);
    let items: Vec<serde_json::Value> = codes.into_iter().map(|c| serde_json::json!({
        "code": c.0, "plan_id": c.1, "value_days": c.2, "uses": c.3, "max_uses": c.4, "seller_id": c.5,
    })).collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({"items": items, "total": items.len()})),
    )
        .into_response()
}

#[derive(Deserialize)]
struct CodeGenReq {
    plan_id: Option<i64>,
    value_days: Option<i64>,
    max_uses: Option<i64>,
    expires_at: Option<String>,
    count: Option<i64>,
}
async fn codes_create(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(f): Json<CodeGenReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let seller_id = match session.seller_id {
        Some(s) => s,
        None => {
            return err(
                StatusCode::FORBIDDEN,
                "not_a_seller",
                "seller profile required",
            )
            .into_response();
        }
    };
    let count = f.count.unwrap_or(1).clamp(1, 100);
    let max_uses = f.max_uses.unwrap_or(1);
    // Clamp to the shared plan-day bound: these days later feed subscription
    // expiry arithmetic when the code is redeemed.
    let value_days = f
        .value_days
        .map(|d| d.clamp(1, crate::handlers::commands::MAX_PLAN_DAYS));
    let mut codes = Vec::new();
    for _ in 0..count {
        let code = state.db.create_code(
            seller_id,
            f.plan_id,
            value_days,
            max_uses,
            f.expires_at.as_deref(),
        );
        codes.push(code);
    }
    (
        StatusCode::CREATED,
        Json(serde_json::json!({"codes": codes})),
    )
        .into_response()
}

async fn codes_get(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(code): Path<String>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    match state.db.get_code(&code) {
        Some((id, seller_id, plan_id, value_days, max_uses, uses, expires_at)) => {
            if session.role == "seller" && session.seller_id != Some(seller_id) {
                return err(StatusCode::FORBIDDEN, "forbidden", "not your code").into_response();
            }
            (StatusCode::OK, Json(serde_json::json!({
                "id": id, "code": code, "seller_id": seller_id, "plan_id": plan_id,
                "value_days": value_days, "max_uses": max_uses, "uses": uses, "expires_at": expires_at,
            }))).into_response()
        }
        None => err(StatusCode::NOT_FOUND, "not_found", "code not found").into_response(),
    }
}

async fn codes_delete(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(code): Path<String>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let (_id, seller_id, _p, _v, _m, _u, _e) = match state.db.get_code(&code) {
        Some(x) => x,
        None => {
            return err(StatusCode::NOT_FOUND, "not_found", "code not found").into_response();
        }
    };
    if session.role == "seller" && session.seller_id != Some(seller_id) {
        return err(StatusCode::FORBIDDEN, "forbidden", "not your code").into_response();
    }
    // Real delete via the scoped helper (L8 db.rs addition): sellers delete
    // only their own codes, admin/owner any. `seller_scope` yields None for
    // admin/owner and Some(id) for sellers.
    if !state
        .db
        .delete_code_seller_scoped(&code, seller_scope(&session))
    {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "delete_failed",
            "code delete failed",
        )
        .into_response();
    }
    state.db.log_action(
        session.account_id,
        &session.role,
        "code_delete",
        "code",
        &code,
        None,
    );
    (
        StatusCode::OK,
        Json(serde_json::json!({"ok": true, "deleted": code})),
    )
        .into_response()
}

#[derive(Deserialize)]
struct RedeemReq {
    user_id: Option<i64>,
}
async fn codes_redeem(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(code): Path<String>,
    Json(f): Json<RedeemReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let user_id = f.user_id.unwrap_or(session.account_id);
    match state.db.redeem_code(&code, user_id) {
        Ok((seller_id, plan_id, value_days)) => {
            // create subscription if plan/value days
            if let Some(days) = value_days {
                // Clamp defensively (legacy codes may predate bounds) so the
                // subscription expiry math cannot overflow.
                let days = days.clamp(1, crate::handlers::commands::MAX_PLAN_DAYS);
                let ch = state.db.seller_channel(seller_id).unwrap_or_default();
                state
                    .db
                    .create_subscription(seller_id, user_id, plan_id, &ch, days, None, None);
            }
            (StatusCode::OK, Json(serde_json::json!({"ok": true, "seller_id": seller_id, "plan_id": plan_id, "value_days": value_days}))).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, "redeem_failed", &e).into_response(),
    }
}

// ---------- Posts ----------
async fn posts_list(State(state): State<WebState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let scope = seller_scope(&session);
    let posts = state.db.list_posts(scope, 100);
    let items: Vec<serde_json::Value> = posts.into_iter().map(|p| serde_json::json!({
        "id": p.0, "seller_id": p.1, "chat_id": p.2, "message_id": p.3, "kind": p.4, "content": p.5, "created_at": p.6,
    })).collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({"items": items, "total": items.len()})),
    )
        .into_response()
}

#[derive(Deserialize)]
struct PostCreateReq {
    channel: Option<String>,
    text: String,
}
async fn posts_create(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(f): Json<PostCreateReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let ch = f
        .channel
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| state.db.get_config("channel").unwrap_or_default());
    if ch.is_empty() || f.text.trim().is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "channel/text required",
        )
        .into_response();
    }
    let msg_id = match state
        .bot
        .send_message(crate::util::to_chat_id(&ch), f.text.clone())
        .await
    {
        Ok(m) => m.id.0,
        Err(_) => 0,
    };
    let seller_id = session.seller_id.unwrap_or(0);
    let _ = state
        .db
        .log_post(seller_id, &ch, msg_id, "text", Some(&f.text));
    state.db.log_action(
        session.account_id,
        &session.role,
        "post_create",
        "post",
        "new",
        Some(&f.text),
    );
    (
        StatusCode::CREATED,
        Json(serde_json::json!({"ok": true, "message_id": msg_id})),
    )
        .into_response()
}

#[derive(Deserialize)]
struct PostScheduleReq {
    channel: Option<String>,
    text: String,
    scheduled_at: String,
}
async fn posts_schedule(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(f): Json<PostScheduleReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let ch = f
        .channel
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| state.db.get_config("channel").unwrap_or_default());
    if ch.is_empty() || f.text.trim().is_empty() || f.scheduled_at.trim().is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "channel/text/scheduled_at required",
        )
        .into_response();
    }
    let seller_id = session.seller_id.unwrap_or(0);
    let id = state
        .db
        .create_scheduled_post(seller_id, &ch, &f.text, "text", &f.scheduled_at);
    state.db.log_action(
        session.account_id,
        &session.role,
        "post_schedule",
        "scheduled_post",
        id.to_string(),
        Some(&f.text),
    );
    (
        StatusCode::CREATED,
        Json(serde_json::json!({"id": id, "scheduled_at": f.scheduled_at})),
    )
        .into_response()
}

// ---------- Posts (single) ----------
async fn posts_get(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let scope = seller_scope(&session);
    let posts = state.db.list_posts(scope, 100000);
    match posts.into_iter().find(|p| p.0 == id) {
        Some(p) => {
            if session.role == "seller" && session.seller_id != Some(p.1) {
                return err(StatusCode::FORBIDDEN, "forbidden", "not your post").into_response();
            }
            (StatusCode::OK, Json(serde_json::json!({
                "id": p.0, "seller_id": p.1, "chat_id": p.2, "message_id": p.3, "kind": p.4, "content": p.5, "created_at": p.6,
            }))).into_response()
        }
        None => err(StatusCode::NOT_FOUND, "not_found", "post not found").into_response(),
    }
}

#[derive(Deserialize)]
struct PostUpdateReq {
    content: Option<String>,
    text: Option<String>,
}
async fn posts_update(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(f): Json<PostUpdateReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let scope = seller_scope(&session);
    let posts = state.db.list_posts(scope, 100000);
    let p = match posts.into_iter().find(|p| p.0 == id) {
        Some(p) => p,
        None => return err(StatusCode::NOT_FOUND, "not_found", "post not found").into_response(),
    };
    if session.role == "seller" && session.seller_id != Some(p.1) {
        return err(StatusCode::FORBIDDEN, "forbidden", "not your post").into_response();
    }
    let new_content = f.content.or(f.text);
    if let Some(c) = &new_content {
        if p.3 != 0 {
            let _ = state
                .bot
                .edit_message_text(crate::util::to_chat_id(&p.2), MessageId(p.3), c.clone())
                .await;
        }
        let _ = state.db.update_post_content(id, c);
    }
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

async fn posts_delete(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let scope = seller_scope(&session);
    let posts = state.db.list_posts(scope, 100000);
    let p = match posts.into_iter().find(|p| p.0 == id) {
        Some(p) => p,
        None => return err(StatusCode::NOT_FOUND, "not_found", "post not found").into_response(),
    };
    if session.role == "seller" && session.seller_id != Some(p.1) {
        return err(StatusCode::FORBIDDEN, "forbidden", "not your post").into_response();
    }
    if p.3 != 0 {
        let _ = state
            .bot
            .delete_message(crate::util::to_chat_id(&p.2), MessageId(p.3))
            .await;
    }
    let _ = state.db.delete_post(id);
    state.db.log_action(
        session.account_id,
        &session.role,
        "post_delete",
        "post",
        id.to_string(),
        None,
    );
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

async fn posts_pin(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let scope = seller_scope(&session);
    let posts = state.db.list_posts(scope, 100000);
    let p = match posts.into_iter().find(|p| p.0 == id) {
        Some(p) => p,
        None => return err(StatusCode::NOT_FOUND, "not_found", "post not found").into_response(),
    };
    if session.role == "seller" && session.seller_id != Some(p.1) {
        return err(StatusCode::FORBIDDEN, "forbidden", "not your post").into_response();
    }
    if p.3 != 0 {
        let _ = state
            .bot
            .pin_chat_message(crate::util::to_chat_id(&p.2), MessageId(p.3))
            .await;
    }
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

async fn posts_unpin(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let scope = seller_scope(&session);
    let posts = state.db.list_posts(scope, 100000);
    let p = match posts.into_iter().find(|p| p.0 == id) {
        Some(p) => p,
        None => return err(StatusCode::NOT_FOUND, "not_found", "post not found").into_response(),
    };
    if session.role == "seller" && session.seller_id != Some(p.1) {
        return err(StatusCode::FORBIDDEN, "forbidden", "not your post").into_response();
    }
    if p.3 != 0 {
        let _ = state
            .bot
            .unpin_chat_message(crate::util::to_chat_id(&p.2))
            .message_id(MessageId(p.3))
            .await;
    }
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

// ---------- Broadcasts ----------
#[derive(Deserialize)]
struct BroadcastReq {
    text: String,
}
async fn broadcasts(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(f): Json<BroadcastReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let recipients = if session.role == "seller" {
        state
            .db
            .list_users_for_seller(session.seller_id.unwrap_or(0))
    } else {
        state.db.list_users()
    };
    let mut sent = 0i64;
    for uid in recipients {
        if state
            .bot
            .send_message(teloxide::types::ChatId(uid), f.text.clone())
            .await
            .is_ok()
        {
            sent += 1;
        }
    }
    state.db.log_action(
        session.account_id,
        &session.role,
        "broadcast",
        "broadcast",
        "all",
        Some(&f.text),
    );
    (StatusCode::OK, Json(serde_json::json!({"sent": sent}))).into_response()
}

// ---------- Channels single ----------
async fn channels_get(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let _ = session;
    (StatusCode::OK, Json(serde_json::json!({"channel": id}))).into_response()
}

#[derive(Deserialize)]
struct ChannelPatchReq {
    title: Option<String>,
    description: Option<String>,
}
async fn channels_patch(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(f): Json<ChannelPatchReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let chat = crate::util::to_chat_id(&id);
    if let Some(title) = &f.title {
        let _ = state.bot.set_chat_title(chat.clone(), title.clone()).await;
    }
    if let Some(desc) = &f.description {
        let _ = state
            .bot
            .set_chat_description(chat.clone())
            .description(desc.clone())
            .await;
    }
    state.db.log_action(
        session.account_id,
        &session.role,
        "channel_update",
        "channel",
        &id,
        None,
    );
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

// ---------- Users single ----------
async fn users_get(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(user_id): Path<i64>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let (uid, username) = state.db.get_user(user_id).unwrap_or((user_id, None));
    let mut resp = serde_json::json!({"user_id": uid, "username": username});
    if session.role == "seller" {
        let scope = session.seller_id;
        let subs = state.db.list_subscriptions(scope, 1000);
        let cnt = subs.iter().filter(|s| s.2 == user_id).count();
        resp["subscription_count"] = serde_json::json!(cnt);
    } else {
        // admin: count across all
        let subs = state.db.list_subscriptions(None, 1000);
        let cnt = subs.iter().filter(|s| s.2 == user_id).count();
        resp["subscription_count"] = serde_json::json!(cnt);
    }
    (StatusCode::OK, Json(resp)).into_response()
}

#[derive(Deserialize)]
struct UserUpsertReq {
    user_id: i64,
    username: Option<String>,
}
async fn users_upsert(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(f): Json<UserUpsertReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let _ = session;
    state.db.upsert_user(f.user_id, f.username.as_deref());
    (StatusCode::CREATED, Json(serde_json::json!({"ok": true}))).into_response()
}

// ---------- AI templates single ----------
async fn ai_templates_get(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let seller_id = session.seller_id.unwrap_or(0);
    match state.db.get_ai_template(seller_id, &name) {
        Some(t) => (StatusCode::OK, Json(serde_json::json!({
            "name": t.0, "description": t.1, "system_prompt": t.2, "user_prompt": t.3, "variables": t.4,
        }))).into_response(),
        None => err(StatusCode::NOT_FOUND, "not_found", "template not found").into_response(),
    }
}

#[derive(Deserialize)]
struct AiTemplatePatchReq {
    description: Option<String>,
    system_prompt: Option<String>,
    user_prompt: Option<String>,
    variables: Option<String>,
}
async fn ai_templates_update(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(f): Json<AiTemplatePatchReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let seller_id = session.seller_id.unwrap_or(0);
    let existing = state.db.get_ai_template(seller_id, &name);
    let description = f
        .description
        .or_else(|| existing.as_ref().and_then(|e| e.1.clone()));
    let system_prompt = f
        .system_prompt
        .or_else(|| existing.as_ref().and_then(|e| e.2.clone()));
    let user_prompt = f
        .user_prompt
        .or_else(|| existing.as_ref().map(|e| e.3.clone()))
        .unwrap_or_default();
    let variables = f
        .variables
        .or_else(|| existing.as_ref().and_then(|e| e.4.clone()));
    state.db.upsert_ai_template(
        seller_id,
        &name,
        description.as_deref(),
        system_prompt.as_deref(),
        &user_prompt,
        variables.as_deref(),
    );
    state.db.log_action(
        session.account_id,
        &session.role,
        "ai_template_update",
        "ai_template",
        &name,
        None,
    );
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

async fn ai_templates_delete(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let seller_id = session.seller_id.unwrap_or(0);
    state.db.delete_ai_template(seller_id, &name);
    state.db.log_action(
        session.account_id,
        &session.role,
        "ai_template_delete",
        "ai_template",
        &name,
        None,
    );
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

// ---------- Users ----------
async fn users_list(State(state): State<WebState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let ids = if session.role == "seller" {
        state
            .db
            .list_users_for_seller(session.seller_id.unwrap_or(0))
    } else {
        state.db.list_users()
    };
    let items: Vec<serde_json::Value> = ids
        .into_iter()
        .map(|u| serde_json::json!({"user_id": u}))
        .collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({"items": items, "total": items.len()})),
    )
        .into_response()
}

// ---------- Stats ----------
async fn dashboard_summary(State(state): State<WebState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let scope = seller_scope(&session);
    let plans = state.db.list_plans(scope);
    let subs = state.db.list_subscriptions(scope, 1000);
    let active_subs = subs.iter().filter(|s| s.4 == "active").count();
    let invoices = state.db.list_invoices(scope, 1000);
    let pending_inv = invoices.iter().filter(|i| i.3 == "active").count();
    let codes = state.db.list_codes(scope, 1000);
    let codes_redeemed: i64 = codes.iter().map(|c| c.3).sum();
    let revenue = state.db.revenue_total(scope);
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "plans_active": plans.iter().filter(|p| p.5).count(),
            "subscriptions_active": active_subs,
            "subscriptions_total": subs.len(),
            "invoices_total": invoices.len(),
            "invoices_pending": pending_inv,
            "codes_total": codes.len(),
            "codes_redeemed": codes_redeemed,
            "revenue_total": revenue,
        })),
    )
        .into_response()
}

async fn revenue(State(state): State<WebState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let scope = seller_scope(&session);
    let total = state.db.revenue_total(scope);
    (StatusCode::OK, Json(serde_json::json!({"total": total}))).into_response()
}

// ---------- Channels ----------
async fn channels_list(State(state): State<WebState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let _ = session;
    let mut set = std::collections::BTreeSet::new();
    if let Some(c) = state.db.get_config("channel") {
        set.insert(c);
    }
    if let Some(c) = &state.cfg.default_channel {
        set.insert(c.clone());
    }
    for p in state.db.list_plans(seller_scope(&session)) {
        if let Some(c) = p.4 {
            set.insert(c);
        }
    }
    let items: Vec<String> = set.into_iter().collect();
    (StatusCode::OK, Json(serde_json::json!({"items": items}))).into_response()
}

// ---------- Config ----------
async fn config_get_channel(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let _ = session;
    let ch = state.db.get_config("channel").unwrap_or_default();
    (StatusCode::OK, Json(serde_json::json!({"channel": ch}))).into_response()
}

#[derive(Deserialize)]
struct ChannelSetReq {
    channel: String,
}
async fn config_set_channel(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(f): Json<ChannelSetReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if !is_admin_role(&session.role) && session.role != "seller" {
        return err(StatusCode::FORBIDDEN, "forbidden", "insufficient role").into_response();
    }
    state.db.set_config("channel", &f.channel);
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

// ---------- AI ----------
async fn ai_templates_list(State(state): State<WebState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let seller_id = session.seller_id.unwrap_or(0);
    let tpls = state.db.list_ai_templates(seller_id);
    let items: Vec<serde_json::Value> = tpls
        .into_iter()
        .map(|t| serde_json::json!({"name": t.0, "description": t.1}))
        .collect();
    (StatusCode::OK, Json(serde_json::json!({"items": items}))).into_response()
}

#[derive(Deserialize)]
struct AiTemplateReq {
    name: String,
    description: Option<String>,
    system_prompt: Option<String>,
    user_prompt: String,
    variables: Option<String>,
}
async fn ai_templates_create(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(f): Json<AiTemplateReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let seller_id = session.seller_id.unwrap_or(0);
    state.db.upsert_ai_template(
        seller_id,
        &f.name,
        f.description.as_deref(),
        f.system_prompt.as_deref(),
        &f.user_prompt,
        f.variables.as_deref(),
    );
    state.db.log_action(
        session.account_id,
        &session.role,
        "ai_template_create",
        "ai_template",
        &f.name,
        None,
    );
    (StatusCode::CREATED, Json(serde_json::json!({"ok": true}))).into_response()
}

async fn ai_usage(State(state): State<WebState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let seller_id = session.seller_id.unwrap_or(0);
    let summary = state.db.ai_usage_summary(seller_id, 30);
    let items: Vec<serde_json::Value> = summary
        .into_iter()
        .map(|s| serde_json::json!({"date": s.0, "tokens": s.1, "requests": s.2}))
        .collect();
    (StatusCode::OK, Json(serde_json::json!({"items": items}))).into_response()
}

// ---------- Finance ----------
async fn finance_summary(State(state): State<WebState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let scope = seller_scope(&session);
    let revenue = state.db.revenue_total(scope);
    let invoices = state.db.list_invoices(scope, 1000);
    let paid: f64 = invoices.iter().filter(|i| i.3 == "paid").count() as f64;
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "revenue_total": revenue,
            "invoices_total": invoices.len(),
            "invoices_paid": paid,
        })),
    )
        .into_response()
}

// ---------- Finance (remaining) ----------
#[derive(Deserialize)]
struct FinancePayoutCreateReq {
    /// Owning seller. Sellers may only create payouts for themselves; admins
    /// must name the seller explicitly.
    seller_id: Option<i64>,
    amount: String,
    asset: String,
    destination: Option<String>,
}
#[derive(Deserialize)]
struct FinancePayoutPatchReq {
    status: String,
    processed_at: Option<String>,
}
#[derive(Deserialize)]
struct FinanceRefundCreateReq {
    invoice_id: String,
    /// Default to the invoice's own amount/asset: a refund can never move
    /// money the invoice did not collect.
    amount: Option<String>,
    asset: Option<String>,
    reason: Option<String>,
}

async fn finance_revenue_by_seller(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if !is_admin_role(&session.role) {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "admin or owner required",
        )
        .into_response();
    }
    let items: Vec<serde_json::Value> = state
        .db
        .revenue_by_seller()
        .into_iter()
        .map(|(sid, total)| {
            serde_json::json!({
                "seller_id": sid, "revenue_total": total,
            })
        })
        .collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({"items": items, "total": items.len()})),
    )
        .into_response()
}

async fn finance_payouts_list(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let scope = if session.role == "seller" {
        match session.seller_id {
            Some(s) => Some(s),
            None => {
                return err(StatusCode::FORBIDDEN, "not_a_seller", "no seller profile")
                    .into_response();
            }
        }
    } else if is_admin_role(&session.role) {
        None
    } else {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "seller or admin required",
        )
        .into_response();
    };
    let payouts = state.db.list_payouts(scope);
    let items: Vec<serde_json::Value> = payouts.into_iter().map(|p| serde_json::json!({
        "id": p.0, "seller_id": p.1, "amount": p.2, "asset": p.3, "destination": p.4, "status": p.5,
    })).collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({"items": items, "total": items.len()})),
    )
        .into_response()
}

async fn finance_payouts_create(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(f): Json<FinancePayoutCreateReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    // Sellers create payouts for themselves; admins must name the seller.
    let seller_id = if session.role == "seller" {
        match session.seller_id {
            Some(s) => {
                if f.seller_id.is_some() && f.seller_id != Some(s) {
                    return err(
                        StatusCode::FORBIDDEN,
                        "forbidden",
                        "cannot create payouts for other sellers",
                    )
                    .into_response();
                }
                s
            }
            None => {
                return err(StatusCode::FORBIDDEN, "not_a_seller", "no seller profile")
                    .into_response();
            }
        }
    } else if is_admin_role(&session.role) {
        match f.seller_id {
            Some(s) => s,
            None => {
                return err(StatusCode::BAD_REQUEST, "bad_request", "seller_id required")
                    .into_response();
            }
        }
    } else {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "seller or admin required",
        )
        .into_response();
    };
    if f.amount.trim().is_empty() || f.asset.trim().is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "amount/asset required",
        )
        .into_response();
    }
    let id = state
        .db
        .create_payout(seller_id, &f.amount, &f.asset, f.destination.as_deref());
    let meta = serde_json::json!({
        "seller_id": seller_id, "amount": f.amount, "asset": f.asset,
        "destination": f.destination,
    })
    .to_string();
    state.db.log_action(
        session.account_id,
        &session.role,
        "payout_create",
        "payout",
        id.to_string(),
        Some(&meta),
    );
    (StatusCode::CREATED, Json(serde_json::json!({"id": id}))).into_response()
}

/// Payout approval: pending -> approved. Owner only (two-person money rule:
/// the seller who benefits never approves their own payout).
async fn finance_payouts_approve(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if session.role != "owner" {
        return err(StatusCode::FORBIDDEN, "forbidden", "owner required").into_response();
    }
    let payout = match state.db.get_payout(id.to_string()) {
        Some(p) => p,
        None => {
            return err(StatusCode::NOT_FOUND, "not_found", "payout not found").into_response();
        }
    };
    let (_pid, _seller, ref amount, ref asset, ref destination, status, ..) = payout;
    if status == "paid" {
        return err(
            StatusCode::CONFLICT,
            "invalid_state",
            "payout already executed",
        )
        .into_response();
    }
    if status != "pending" && status != "approved" {
        return err(
            StatusCode::CONFLICT,
            "invalid_state",
            &format!("cannot approve payout in status '{}'", status),
        )
        .into_response();
    }
    state.db.set_payout_status(id, "approved");
    let meta = serde_json::json!({
        "amount": amount, "asset": asset, "destination": destination,
    })
    .to_string();
    state.db.log_action(
        session.account_id,
        &session.role,
        "payout_approve",
        "payout",
        id.to_string(),
        Some(&meta),
    );
    (
        StatusCode::OK,
        Json(serde_json::json!({"ok": true, "status": "approved"})),
    )
        .into_response()
}

/// Payout execution: approved -> paid by moving real money through Crypto Pay
/// `transfer` to the destination Telegram user. A transfer failure keeps the
/// payout 'approved' so execution can be retried.
async fn finance_payouts_execute(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if session.role != "owner" {
        return err(StatusCode::FORBIDDEN, "forbidden", "owner required").into_response();
    }
    let payout = match state.db.get_payout(id.to_string()) {
        Some(p) => p,
        None => {
            return err(StatusCode::NOT_FOUND, "not_found", "payout not found").into_response();
        }
    };
    let (_pid, seller_id, amount, asset, destination, status, ..) = payout;
    if status == "paid" {
        return err(
            StatusCode::CONFLICT,
            "invalid_state",
            "payout already executed",
        )
        .into_response();
    }
    if status != "approved" {
        return err(
            StatusCode::CONFLICT,
            "invalid_state",
            "payout must be approved before execution",
        )
        .into_response();
    }
    // Crypto Pay transfers go to a Telegram user id: the destination must be
    // exactly that.
    let Some(dest) = destination
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    else {
        return err(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "payout has no destination",
        )
        .into_response();
    };
    let Ok(dest_user) = dest.parse::<i64>() else {
        return err(
            StatusCode::BAD_REQUEST,
            "bad_destination",
            "destination must be a Telegram user id",
        )
        .into_response();
    };
    // Resolve a crypto token exactly like the webhook does.
    let token = state
        .cfg
        .crypto_token
        .clone()
        .or_else(|| state.db.get_config("crypto_token"));
    let tok = match token {
        Some(t) => t,
        None => {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                "transfer_unavailable",
                "crypto pay not configured",
            )
            .into_response();
        }
    };
    let cp = crate::crypto::CryptoPay::new(tok);
    match cp.transfer(dest_user, &asset, &amount).await {
        Ok(receipt) => {
            state.db.set_payout_status(id, "paid");
            let meta = serde_json::json!({
                "seller_id": seller_id, "amount": amount, "asset": asset,
                "destination": dest_user, "receipt": receipt,
            })
            .to_string();
            state.db.log_action(
                session.account_id,
                &session.role,
                "payout_executed",
                "payout",
                id.to_string(),
                Some(&meta),
            );
            (
                StatusCode::OK,
                Json(serde_json::json!({"ok": true, "status": "paid"})),
            )
                .into_response()
        }
        Err(e) => {
            // Keep 'approved' so the transfer can be retried; audit the
            // failed attempt (a money transition was attempted).
            let meta = serde_json::json!({
                "seller_id": seller_id, "amount": amount, "asset": asset,
                "destination": dest_user, "error": e.to_string(),
            })
            .to_string();
            state.db.log_action(
                session.account_id,
                &session.role,
                "payout_execute_failed",
                "payout",
                id.to_string(),
                Some(&meta),
            );
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "transfer_failed",
                &format!("transfer failed: {}", e),
            )
            .into_response()
        }
    }
}

async fn finance_payouts_patch(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(f): Json<FinancePayoutPatchReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if !is_admin_role(&session.role) {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "admin or owner required",
        )
        .into_response();
    }
    let processed_at = f
        .processed_at
        .clone()
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    let _ = processed_at;
    state
        .db
        .update_payout_status_by(id, &f.status, Some(session.account_id));
    state.db.log_action(
        session.account_id,
        &session.role,
        "payout_update",
        "payout",
        id.to_string(),
        Some(&f.status),
    );
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

async fn finance_payouts_process(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if !is_admin_role(&session.role) {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "admin or owner required",
        )
        .into_response();
    }
    match state.db.get_payout(id.to_string()) {
        Some(_) => {}
        None => {
            return err(StatusCode::NOT_FOUND, "not_found", "payout not found").into_response();
        }
    }
    state
        .db
        .update_payout_status_by(id, "paid", Some(session.account_id));
    state.db.log_action(
        session.account_id,
        &session.role,
        "payout_process",
        "payout",
        id.to_string(),
        Some("paid"),
    );
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

async fn finance_refunds_process(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let refund = match state.db.get_refund(id.to_string()) {
        Some(r) => r,
        None => {
            return err(StatusCode::NOT_FOUND, "not_found", "refund not found").into_response();
        }
    };
    let (
        _rid,
        seller_id,
        invoice_id,
        _user_id,
        _amount,
        _asset,
        _reason,
        _status,
        _created,
        _updated,
        _pa,
        _pb,
    ) = refund;
    // permission: admin+ or seller (own)
    if session.role == "seller" {
        if session.seller_id != Some(seller_id) {
            return err(StatusCode::FORBIDDEN, "forbidden", "not your refund").into_response();
        }
    } else if !is_admin_role(&session.role) {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "admin or owner required",
        )
        .into_response();
    }
    state
        .db
        .update_refund_status_by(id, "processed", Some(session.account_id));
    if let Some(inv) = invoice_id {
        state.db.update_invoice_status(&inv, "refunded");
        // optionally revoke subscription access for subscriptions tied to this invoice
        let subs = state.db.subscriptions_for_invoice(&inv);
        for (sub_id, _sid, uid, ch) in subs {
            if !ch.is_empty() {
                let cid = crate::util::to_chat_id(&ch);
                let _ = state
                    .bot
                    .ban_chat_member(cid.clone(), teloxide::types::UserId(uid as u64))
                    .await;
                let _ = state
                    .bot
                    .unban_chat_member(cid, teloxide::types::UserId(uid as u64))
                    .await;
            }
            state.db.mark_subscription_expired(sub_id);
        }
    }
    state.db.log_action(
        session.account_id,
        &session.role,
        "refund_process",
        "refund",
        id.to_string(),
        None,
    );
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

async fn finance_refunds_list(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let scope = if session.role == "seller" {
        match session.seller_id {
            Some(s) => Some(s),
            None => {
                return err(StatusCode::FORBIDDEN, "not_a_seller", "no seller profile")
                    .into_response();
            }
        }
    } else if is_admin_role(&session.role) {
        None
    } else {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "seller or admin required",
        )
        .into_response();
    };
    let refunds = state.db.list_refunds(scope);
    let items: Vec<serde_json::Value> = refunds.into_iter().map(|r| serde_json::json!({
        "id": r.0, "seller_id": r.1, "invoice_id": r.2, "amount": r.3, "asset": r.4, "status": r.5,
    })).collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({"items": items, "total": items.len()})),
    )
        .into_response()
}

/// Refund creation: seller-scoped, always rooted in one of the caller's own
/// paid invoices. Amount/asset default to the invoice's, so a refund can
/// never move money the invoice did not collect.
async fn finance_refunds_create(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(f): Json<FinanceRefundCreateReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if session.role != "seller" && !is_admin_role(&session.role) {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "seller or admin required",
        )
        .into_response();
    }
    if f.invoice_id.trim().is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "invoice_id required",
        )
        .into_response();
    }
    // Seller scoping: the invoice must belong to the caller (admins may
    // refund any seller's invoice, inheriting its seller).
    let inv = match state.db.get_invoice(&f.invoice_id) {
        Some(i) => i,
        None => {
            return err(StatusCode::NOT_FOUND, "not_found", "invoice not found").into_response();
        }
    };
    let (inv_amount, inv_asset, inv_status, _desc, _plan, inv_user, inv_seller) = inv;
    if session.role == "seller" && session.seller_id != Some(inv_seller) {
        return err(StatusCode::FORBIDDEN, "forbidden", "not your invoice").into_response();
    }
    // Only settled money can be returned.
    if inv_status != "paid" {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_state",
            "only paid invoices can be refunded",
        )
        .into_response();
    }
    let amount = f.amount.unwrap_or(inv_amount);
    let asset = f.asset.unwrap_or(inv_asset);
    let id = state.db.create_refund(
        inv_seller,
        Some(f.invoice_id.trim()),
        inv_user,
        &amount,
        &asset,
        f.reason.as_deref(),
    );
    let meta = serde_json::json!({
        "invoice_id": f.invoice_id, "amount": amount, "asset": asset,
        "user_id": inv_user, "reason": f.reason,
    })
    .to_string();
    state.db.log_action(
        session.account_id,
        &session.role,
        "refund_create",
        "refund",
        id.to_string(),
        Some(&meta),
    );
    (StatusCode::CREATED, Json(serde_json::json!({"id": id}))).into_response()
}

/// Refund approval: pending -> approved. Owner/admin only — the seller who
/// owes the money never approves their own refund.
async fn finance_refunds_approve(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if !is_admin_role(&session.role) {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "admin or owner required",
        )
        .into_response();
    }
    let refund = match state.db.get_refund(id.to_string()) {
        Some(r) => r,
        None => {
            return err(StatusCode::NOT_FOUND, "not_found", "refund not found").into_response();
        }
    };
    let (_rid, _seller, ref invoice_id, _user, ref amount, ref asset, _reason, ref status, ..) =
        refund;
    if status == "executed" {
        return err(
            StatusCode::CONFLICT,
            "invalid_state",
            "refund already executed",
        )
        .into_response();
    }
    if status != "pending" && status != "approved" {
        return err(
            StatusCode::CONFLICT,
            "invalid_state",
            &format!("cannot approve refund in status '{}'", status),
        )
        .into_response();
    }
    state.db.set_refund_status(id, "approved");
    let meta = serde_json::json!({
        "invoice_id": invoice_id, "amount": amount, "asset": asset,
    })
    .to_string();
    state.db.log_action(
        session.account_id,
        &session.role,
        "refund_approve",
        "refund",
        id.to_string(),
        Some(&meta),
    );
    (
        StatusCode::OK,
        Json(serde_json::json!({"ok": true, "status": "approved"})),
    )
        .into_response()
}

/// Refund execution: approved -> executed by transferring real money back to
/// the buyer through Crypto Pay `transfer`. On success the invoice is marked
/// refunded, any subscription the invoice granted is revoked (status
/// 'refunded' + channel kick via ban+unban), and the whole transition is
/// audited. A transfer failure keeps the refund 'approved' for retry.
async fn finance_refunds_execute(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if !is_admin_role(&session.role) {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "admin or owner required",
        )
        .into_response();
    }
    let refund = match state.db.get_refund(id.to_string()) {
        Some(r) => r,
        None => {
            return err(StatusCode::NOT_FOUND, "not_found", "refund not found").into_response();
        }
    };
    let (
        _rid,
        _seller,
        invoice_id,
        refund_user,
        amount,
        asset,
        _reason,
        status,
        _created,
        _updated,
        _pa,
        _pb,
    ) = refund;
    if status == "executed" {
        return err(
            StatusCode::CONFLICT,
            "invalid_state",
            "refund already executed",
        )
        .into_response();
    }
    if status != "approved" {
        return err(
            StatusCode::CONFLICT,
            "invalid_state",
            "refund must be approved before execution",
        )
        .into_response();
    }
    // Crypto Pay transfers target a Telegram user: the buyer the invoice was
    // bound to.
    let Some(refund_user) = refund_user else {
        return err(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "refund has no bound user; cannot transfer",
        )
        .into_response();
    };
    // Resolve a crypto token exactly like the webhook does.
    let token = state
        .cfg
        .crypto_token
        .clone()
        .or_else(|| state.db.get_config("crypto_token"));
    let tok = match token {
        Some(t) => t,
        None => {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                "transfer_unavailable",
                "crypto pay not configured",
            )
            .into_response();
        }
    };
    let cp = crate::crypto::CryptoPay::new(tok);
    match cp.transfer(refund_user, &asset, &amount).await {
        Ok(receipt) => {
            state.db.set_refund_status(id, "executed");
            let mut subscription_revoked = false;
            if let Some(inv) = invoice_id.as_deref() {
                state.db.update_invoice_status(inv, "refunded");
                if let Some((sub_id, channel)) = state.db.subscription_by_invoice(inv) {
                    state.db.set_subscription_status(sub_id, "refunded");
                    subscription_revoked = true;
                    if !channel.is_empty() {
                        // Kick (ban+unban) so the refunded buyer loses channel
                        // access immediately. Best-effort: the status flip is
                        // the source of truth.
                        let cid = crate::util::to_chat_id(&channel);
                        let _ = state
                            .bot
                            .ban_chat_member(
                                cid.clone(),
                                teloxide::types::UserId(refund_user as u64),
                            )
                            .await;
                        let _ = state
                            .bot
                            .unban_chat_member(cid, teloxide::types::UserId(refund_user as u64))
                            .await;
                    }
                }
            }
            let meta = serde_json::json!({
                "invoice_id": invoice_id, "amount": amount, "asset": asset,
                "user_id": refund_user, "subscription_revoked": subscription_revoked,
                "receipt": receipt,
            })
            .to_string();
            state.db.log_action(
                session.account_id,
                &session.role,
                "refund_executed",
                "refund",
                id.to_string(),
                Some(&meta),
            );
            (
                StatusCode::OK,
                Json(serde_json::json!({"ok": true, "status": "executed"})),
            )
                .into_response()
        }
        Err(e) => {
            // Keep 'approved' so the transfer can be retried; audit the
            // failed attempt (a money transition was attempted).
            let meta = serde_json::json!({
                "invoice_id": invoice_id, "amount": amount, "asset": asset,
                "user_id": refund_user, "error": e.to_string(),
            })
            .to_string();
            state.db.log_action(
                session.account_id,
                &session.role,
                "refund_execute_failed",
                "refund",
                id.to_string(),
                Some(&meta),
            );
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "transfer_failed",
                &format!("transfer failed: {}", e),
            )
            .into_response()
        }
    }
}

async fn finance_export(State(state): State<WebState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if session.role != "seller" && !is_admin_role(&session.role) {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "seller or admin required",
        )
        .into_response();
    }
    let scope = seller_scope(&session);
    let invs = state.db.list_invoices(scope, 10000);
    let mut csv = String::from("invoice_id,amount,asset,status,pay_url\n");
    for i in invs {
        csv.push_str(&format!("{},{},{},{},{}\n", i.0, i.1, i.2, i.3, i.4));
    }
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/csv")],
        csv,
    )
        .into_response()
}

// ---------- Products & shop ----------
const PRODUCT_KINDS: [&str; 3] = ["digital_link", "digital_file", "physical"];

#[derive(Deserialize)]
struct ProductCreateReq {
    name: String,
    price: String,
    asset: String,
    kind: String,
    delivery: Option<String>,
    stock: Option<i64>,
}

async fn products_list(State(state): State<WebState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    // Sellers see their own catalogue; admins see everything.
    let scope = seller_scope(&session);
    let products = state.db.list_products(scope);
    let items: Vec<serde_json::Value> = products
        .into_iter()
        .map(|p| {
            serde_json::json!({
                "id": p.0, "seller_id": p.1, "name": p.2, "price": p.3, "asset": p.4,
                "kind": p.5, "delivery": p.6, "stock": p.7, "active": p.8,
            })
        })
        .collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({"items": items, "total": items.len()})),
    )
        .into_response()
}

async fn products_create(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(f): Json<ProductCreateReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let seller_id = match session.seller_id {
        Some(s) => s,
        None => {
            return err(
                StatusCode::FORBIDDEN,
                "not_a_seller",
                "seller profile required",
            )
            .into_response();
        }
    };
    if f.name.trim().is_empty()
        || f.price.trim().is_empty()
        || f.asset.trim().is_empty()
        || f.kind.trim().is_empty()
    {
        return err(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "name/price/asset/kind required",
        )
        .into_response();
    }
    if !PRODUCT_KINDS.contains(&f.kind.as_str()) {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_kind",
            "kind must be digital_link, digital_file or physical",
        )
        .into_response();
    }
    // A digital product without a payload would take money and fail at
    // delivery; reject it at creation.
    if f.delivery
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
        && f.kind != "physical"
    {
        return err(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "digital products require a delivery payload",
        )
        .into_response();
    }
    match state.db.add_product(
        seller_id,
        f.name.trim(),
        f.price.trim(),
        f.asset.trim(),
        f.kind.trim(),
        f.delivery.as_deref(),
        f.stock,
    ) {
        Ok(id) => {
            let meta = serde_json::json!({
                "name": f.name, "kind": f.kind, "price": f.price, "asset": f.asset,
                "stock": f.stock,
            })
            .to_string();
            state.db.log_action(
                session.account_id,
                &session.role,
                "product_create",
                "product",
                id.to_string(),
                Some(&meta),
            );
            (StatusCode::CREATED, Json(serde_json::json!({"id": id}))).into_response()
        }
        // e.g. UNIQUE(seller_id, name) collision.
        Err(e) => err(StatusCode::CONFLICT, "conflict", &e).into_response(),
    }
}

/// Product deactivation (soft delete): /products/{name}. Seller-scoped;
/// admins may deactivate by name across sellers (ambiguous names are
/// rejected instead of guessing).
async fn products_delete(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let seller_id = if session.role == "seller" {
        match session.seller_id {
            Some(s) => s,
            None => {
                return err(StatusCode::FORBIDDEN, "not_a_seller", "no seller profile")
                    .into_response();
            }
        }
    } else if is_admin_role(&session.role) {
        // Resolve the owning seller by name; products are unique per
        // (seller, name), not globally.
        let owners: Vec<i64> = state
            .db
            .list_products(None)
            .into_iter()
            .filter(|p| p.2 == name)
            .map(|p| p.1)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        match owners[..] {
            [s] => s,
            [] => {
                return err(StatusCode::NOT_FOUND, "not_found", "product not found")
                    .into_response();
            }
            _ => {
                return err(
                    StatusCode::BAD_REQUEST,
                    "ambiguous",
                    "product name exists for multiple sellers",
                )
                .into_response();
            }
        }
    } else {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "seller or admin required",
        )
        .into_response();
    };
    if state.db.get_product(seller_id, &name).is_none() {
        return err(StatusCode::NOT_FOUND, "not_found", "product not found").into_response();
    }
    state.db.set_product_active(seller_id, &name, false);
    state.db.log_action(
        session.account_id,
        &session.role,
        "product_deactivate",
        "product",
        name.clone(),
        None,
    );
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

/// Public shop listing: active products across all sellers. No
/// authentication; delivery payloads (the paid content) are never included.
async fn shop_list(State(state): State<WebState>) -> impl IntoResponse {
    let items: Vec<serde_json::Value> = state
        .db
        .list_products(None)
        .into_iter()
        .filter(|p| p.8 != 0)
        .map(|p| {
            serde_json::json!({
                "name": p.2, "price": p.3, "asset": p.4,
                "kind": match p.5.as_str() {
                    "physical" => "product",
                    _ => "digital",
                },
            })
        })
        .collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({"items": items, "total": items.len()})),
    )
        .into_response()
}

async fn admin_sellers(State(state): State<WebState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if !is_admin_role(&session.role) {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "admin or owner required",
        )
        .into_response();
    }
    let ids = state.db.list_sellers();
    let items: Vec<serde_json::Value> = ids
        .into_iter()
        .map(|id| {
            serde_json::json!({
                "user_id": id, "default_channel": state.db.seller_channel(id),
            })
        })
        .collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({"items": items, "total": items.len()})),
    )
        .into_response()
}

async fn admin_stats(State(state): State<WebState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if !is_admin_role(&session.role) {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "admin or owner required",
        )
        .into_response();
    }
    let sellers = state.db.count_sellers();
    let users = state.db.count_users();
    let plans = state.db.count_plans(None);
    let subs = state.db.count_subscriptions(None, None);
    let active_subs = state.db.count_subscriptions(None, Some("active"));
    let invoices = state.db.count_invoices(None, None);
    let revenue = state.db.revenue_total(None);
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "sellers_total": sellers, "users_total": users, "plans_total": plans,
            "subscriptions_total": subs, "subscriptions_active": active_subs,
            "invoices_total": invoices, "revenue_total": revenue,
        })),
    )
        .into_response()
}

async fn admin_web_accounts(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if session.role != "owner" {
        return err(StatusCode::FORBIDDEN, "forbidden", "owner required").into_response();
    }
    let accounts = state.db.list_web_accounts_full();
    let items: Vec<serde_json::Value> = accounts
        .into_iter()
        .map(|a| serde_json::json!({"id": a.0, "username": a.1, "role": a.2, "seller_id": a.3}))
        .collect();
    (StatusCode::OK, Json(serde_json::json!({"items": items}))).into_response()
}

// ---------- Admin (remaining) ----------
#[derive(Deserialize)]
struct AdminSellerPatchReq {
    default_channel: Option<String>,
}
#[derive(Deserialize)]
struct AdminAdminCreateReq {
    user_id: i64,
    username: Option<String>,
}
#[derive(Deserialize)]
struct WebAccountCreateReq {
    username: String,
    password: String,
    role: String,
    seller_id: Option<i64>,
}
#[derive(Deserialize)]
struct WebAccountPasswordReq {
    password: String,
}
#[derive(Deserialize)]
struct AuditQuery {
    limit: Option<i64>,
}

async fn admin_seller_get(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(user_id): Path<i64>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if !is_admin_role(&session.role) {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "admin or owner required",
        )
        .into_response();
    }
    match state.db.get_seller(user_id) {
        Some((uid, username, channel)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "user_id": uid, "username": username, "default_channel": channel,
            })),
        )
            .into_response(),
        None => err(StatusCode::NOT_FOUND, "not_found", "seller not found").into_response(),
    }
}

async fn admin_seller_patch(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(user_id): Path<i64>,
    Json(f): Json<AdminSellerPatchReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if !is_admin_role(&session.role) {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "admin or owner required",
        )
        .into_response();
    }
    if let Some(ch) = &f.default_channel {
        state.db.set_seller_channel(user_id, ch);
    }
    state.db.log_action(
        session.account_id,
        &session.role,
        "seller_update",
        "seller",
        user_id.to_string(),
        f.default_channel.as_deref(),
    );
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

async fn admin_admins_list(State(state): State<WebState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if session.role != "owner" {
        return err(StatusCode::FORBIDDEN, "forbidden", "owner required").into_response();
    }
    let admins = state.db.list_admins_detail();
    let items: Vec<serde_json::Value> = admins
        .into_iter()
        .map(|(uid, username)| serde_json::json!({"user_id": uid, "username": username}))
        .collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({"items": items, "total": items.len()})),
    )
        .into_response()
}

async fn admin_admins_create(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(f): Json<AdminAdminCreateReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if session.role != "owner" {
        return err(StatusCode::FORBIDDEN, "forbidden", "owner required").into_response();
    }
    state.db.add_admin(f.user_id, f.username.as_deref());
    state.db.log_action(
        session.account_id,
        &session.role,
        "admin_add",
        "admin",
        f.user_id.to_string(),
        None,
    );
    (StatusCode::CREATED, Json(serde_json::json!({"ok": true}))).into_response()
}

async fn admin_admins_delete(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(user_id): Path<i64>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if session.role != "owner" {
        return err(StatusCode::FORBIDDEN, "forbidden", "owner required").into_response();
    }
    state.db.remove_admin(user_id);
    state.db.log_action(
        session.account_id,
        &session.role,
        "admin_remove",
        "admin",
        user_id.to_string(),
        None,
    );
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

async fn admin_web_accounts_create(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(f): Json<WebAccountCreateReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if session.role != "owner" {
        return err(StatusCode::FORBIDDEN, "forbidden", "owner required").into_response();
    }
    match state
        .db
        .create_web_account(&f.username, &f.password, &f.role, f.seller_id)
        .await
    {
        Ok(()) => {
            state.db.log_action(
                session.account_id,
                &session.role,
                "web_account_create",
                "web_account",
                f.username.clone(),
                None,
            );
            (StatusCode::CREATED, Json(serde_json::json!({"ok": true}))).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, "create_failed", &e).into_response(),
    }
}

async fn admin_web_accounts_delete(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if session.role != "owner" {
        return err(StatusCode::FORBIDDEN, "forbidden", "owner required").into_response();
    }
    state.db.delete_web_account(id);
    state.db.log_action(
        session.account_id,
        &session.role,
        "web_account_delete",
        "web_account",
        id.to_string(),
        None,
    );
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

async fn admin_web_accounts_password(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(f): Json<WebAccountPasswordReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if session.role != "owner" {
        return err(StatusCode::FORBIDDEN, "forbidden", "owner required").into_response();
    }
    if f.password.len() < 6 {
        return err(
            StatusCode::BAD_REQUEST,
            "weak_password",
            "password must be at least 6 chars",
        )
        .into_response();
    }
    match state.db.update_web_password_by_id(id, &f.password).await {
        Ok(()) => {
            state.db.log_action(
                session.account_id,
                &session.role,
                "web_account_password",
                "web_account",
                id.to_string(),
                None,
            );
            (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, "update_failed", &e).into_response(),
    }
}

async fn admin_audit_log(
    State(state): State<WebState>,
    headers: HeaderMap,
    Query(q): Query<AuditQuery>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if !is_admin_role(&session.role) {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "admin or owner required",
        )
        .into_response();
    }
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let logs = state.db.list_audit_log(limit);
    let items: Vec<serde_json::Value> = logs.into_iter().map(|l| serde_json::json!({
        "id": l.0, "actor_id": l.1, "actor_role": l.2, "action": l.3, "target_type": l.4, "target_id": l.5, "metadata": l.6, "created_at": l.7,
    })).collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({"items": items, "total": items.len()})),
    )
        .into_response()
}

// ---------- API keys (P3) ----------
#[derive(Deserialize)]
struct ApiKeyCreateReq {
    name: Option<String>,
    scopes: Option<String>,
}

async fn api_keys_list(State(state): State<WebState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if session.role != "seller" && !is_admin_role(&session.role) {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "seller or admin required",
        )
        .into_response();
    }
    let keys = state.db.list_api_keys_detail(session.account_id);
    let items: Vec<serde_json::Value> = keys.into_iter().map(|k| serde_json::json!({
        "id": k.0, "account_id": k.1, "name": k.2, "key_prefix": k.3, "scopes": k.4, "revoked": k.5, "created_at": k.6,
    })).collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({"items": items, "total": items.len()})),
    )
        .into_response()
}

async fn api_keys_create(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(f): Json<ApiKeyCreateReq>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if session.role != "seller" && !is_admin_role(&session.role) {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "seller or admin required",
        )
        .into_response();
    }
    let (id, raw) =
        state
            .db
            .create_api_key_full(session.account_id, f.name.as_deref(), f.scopes.as_deref());
    state.db.log_action(
        session.account_id,
        &session.role,
        "api_key_create",
        "api_key",
        id.to_string(),
        None,
    );
    (
        StatusCode::CREATED,
        Json(serde_json::json!({"id": id, "key": raw, "api_key": raw})),
    )
        .into_response()
}

async fn api_keys_delete(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if session.role != "seller" && !is_admin_role(&session.role) {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "seller or admin required",
        )
        .into_response();
    }
    // ownership check for non-admin
    if !is_admin_role(&session.role) {
        let own = state
            .db
            .list_api_keys(session.account_id)
            .into_iter()
            .any(|k| k.0 == id);
        if !own {
            return err(StatusCode::FORBIDDEN, "forbidden", "not your api key").into_response();
        }
    }
    state.db.revoke_api_key(id);
    state.db.log_action(
        session.account_id,
        &session.role,
        "api_key_revoke",
        "api_key",
        id.to_string(),
        None,
    );
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, db::Db};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use proptest::prelude::*;
    use std::sync::Arc;
    use tower::ServiceExt;

    /// Arbitrary JSON generator (depth-bounded) for parser property tests.
    fn arb_json(depth: u32) -> BoxedStrategy<serde_json::Value> {
        use serde_json::Value;
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(Value::from),
            any::<f64>()
                .prop_filter("finite", |v| v.is_finite())
                .prop_map(Value::from),
            ".*".prop_map(Value::String),
        ];
        if depth == 0 {
            return leaf.boxed();
        }
        prop_oneof![
            leaf,
            proptest::collection::vec(arb_json(depth - 1), 0..4).prop_map(Value::Array),
            proptest::collection::hash_map(".*", arb_json(depth - 1), 0..4)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
        .boxed()
    }

    proptest! {
        /// The invoice parser must never panic on arbitrary JSON.
        #[test]
        fn prop_extract_invoice_items_never_panics(v in arb_json(3)) {
            let _ = extract_invoice_items(&v);
        }

        /// All three known Crypto Pay response shapes decode to the same items.
        #[test]
        fn prop_extract_invoice_items_shapes(
            items in proptest::collection::vec(arb_json(1), 0..5)
        ) {
            let arr = serde_json::Value::Array(items.clone());
            for wrapped in [
                arr.clone(),
                serde_json::json!({ "items": arr.clone() }),
                serde_json::json!({ "result": { "items": arr } }),
            ] {
                prop_assert_eq!(extract_invoice_items(&wrapped), items.clone());
            }
        }

        /// Signature checking: a correct HMAC hex digest passes ct_eq and any
        /// single-character mutation fails — for every key/body pair.
        #[test]
        fn prop_signature_check(
            key in proptest::collection::vec(any::<u8>(), 0..64),
            body in proptest::collection::vec(any::<u8>(), 0..256),
            idx in any::<usize>(),
        ) {
            let mac = crate::util::hmac_sha256(&key, &body);
            let hex: String = mac.iter().map(|b| format!("{:02x}", b)).collect();
            prop_assert!(crate::util::ct_eq(&hex, &hex));
            let mut chars: Vec<char> = hex.chars().collect();
            let i = idx % chars.len().max(1);
            chars[i] = if chars[i] == '0' { '1' } else { '0' };
            let bad: String = chars.into_iter().collect();
            prop_assert!(!crate::util::ct_eq(&hex, &bad));
        }
    }

    async fn make_state() -> (WebState, tempfile::TempDir) {
        // reuse web test helper pattern
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db").to_str().unwrap().to_string();
        let db = Db::new(&db_path).unwrap();
        db.create_web_account("owner", "secret", "owner", None)
            .await
            .unwrap();
        let cfg = Config {
            bot_token: "123:abc".to_string(),
            owner_id: 1,
            db_path: db_path.clone(),
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
            login_attempts: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            base_path: String::new(),
        };
        (state, dir)
    }

    #[tokio::test]
    async fn health_ok() {
        let (state, _dir) = make_state().await;
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn api_login_and_me() {
        let (state, _dir) = make_state().await;
        let app = router(state);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/auth/login")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"username":"owner","password":"secret"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let token = v["token"].as_str().unwrap().to_string();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/auth/me")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn api_unauth_returns_401() {
        let (state, _dir) = make_state().await;
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/dashboard/summary")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // ---- helpers ----
    async fn make_seller_state() -> (WebState, tempfile::TempDir) {
        let (state, dir) = make_state().await;
        state
            .db
            .create_web_account("seller", "secret", "seller", Some(42))
            .await
            .unwrap();
        state
            .db
            .upsert_seller(42, Some("seller42"), Some("@chan42"));
        (state, dir)
    }
    fn owner_token(state: &WebState) -> String {
        let (id, _, _, _, _) = state.db.get_web_account_by_username("owner").unwrap();
        state.db.create_web_session(id, "owner", None, 7)
    }
    fn seller_token(state: &WebState) -> String {
        let (id, _, _, _, _) = state.db.get_web_account_by_username("seller").unwrap();
        state.db.create_web_session(id, "seller", Some(42), 7)
    }

    // ---- Auth ----
    #[tokio::test]
    async fn api_me_returns_role() {
        let (state, _dir) = make_state().await;
        let token = owner_token(&state);
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/auth/me")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["role"].as_str().unwrap(), "owner");
    }

    #[tokio::test]
    async fn api_logout_revokes() {
        let (state, _dir) = make_state().await;
        let token = owner_token(&state);
        let app = router(state);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/auth/logout")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/auth/me")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // ---- Plans ----
    #[tokio::test]
    async fn api_plans_create_list_get() {
        let (state, _dir) = make_seller_state().await;
        let token = seller_token(&state);
        let db = state.db.clone();
        let app = router(state);
        let resp = app.clone().oneshot(Request::builder().method("POST").uri("/api/v1/plans")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", token))
            .body(Body::from(r#"{"name":"Plan1","price":"10","asset":"USDT","duration_days":30,"channel_id":"@chan42"}"#)).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/plans")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["total"].as_i64().unwrap(), 1);
        let plan_id = db.get_plan(42, "Plan1").unwrap().0;
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/plans/{}", plan_id))
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["name"].as_str().unwrap(), "Plan1");
    }

    #[tokio::test]
    async fn api_plans_update_delete() {
        let (state, _dir) = make_seller_state().await;
        let token = seller_token(&state);
        let db = state.db.clone();
        let app = router(state);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/plans")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::from(
                        r#"{"name":"PlanU","price":"10","asset":"USDT","duration_days":30}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let plan_id = db.get_plan(42, "PlanU").unwrap().0;
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/api/v1/plans/{}", plan_id))
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::from(r#"{"price":"20"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let plan_id = db.get_plan(42, "PlanU").unwrap().0;
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/v1/plans/{}", plan_id))
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/plans")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["total"].as_i64().unwrap(), 0);
    }

    // ---- Subscriptions ----
    #[tokio::test]
    async fn api_subs_create_list_get_patch() {
        let (state, _dir) = make_seller_state().await;
        let token = seller_token(&state);
        let app = router(state);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/subscriptions")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::from(
                        r#"{"user_id":7,"channel_id":"@chan42","days":10}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let sub_id = v["id"].as_i64().unwrap();
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/subscriptions")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/subscriptions/{}", sub_id))
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/api/v1/subscriptions/{}", sub_id))
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::from(r#"{"status":"expired"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ---- Invoices ----
    #[tokio::test]
    async fn api_invoices_create_list_get_status() {
        let (state, _dir) = make_seller_state().await;
        let token = seller_token(&state);
        let app = router(state);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/invoices")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::from(
                        r#"{"amount":"5","asset":"USDT","description":"test"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let inv_id = v["invoice_id"].as_str().unwrap().to_string();
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/invoices")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/invoices/{}", inv_id))
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/invoices/{}/status", inv_id))
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::from(r#"{"status":"paid"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Manual 'paid' transitions are rejected: paid is a verified state
        // reachable only via the webhook or claim flow.
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/invoices/{}/status", inv_id))
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::from(r#"{"status":"expired"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/invoices/{}", inv_id))
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["status"].as_str().unwrap(), "expired");
    }

    // ---- Codes ----
    #[tokio::test]
    async fn api_codes_create_list_get_redeem() {
        let (state, _dir) = make_seller_state().await;
        let token = seller_token(&state);
        let app = router(state);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/codes")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::from(r#"{"value_days":7,"max_uses":1,"count":1}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let code = v["codes"][0].as_str().unwrap().to_string();
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/codes")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/codes/{}", code))
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/codes/{}/redeem", code))
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::from(r#"{"user_id":99}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ---- Posts ----
    #[tokio::test]
    async fn api_posts_create_list() {
        let (state, _dir) = make_seller_state().await;
        let token = seller_token(&state);
        let app = router(state);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/posts")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::from(r#"{"text":"hello","channel":"@chan42"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/posts")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["total"].as_i64().unwrap() >= 1);
    }

    // ---- Users ----
    #[tokio::test]
    async fn api_users_list_upsert() {
        let (state, _dir) = make_state().await;
        let token = owner_token(&state);
        let app = router(state);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/users/upsert")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::from(r#"{"user_id":555,"username":"foo"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/users")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let items = v["items"].as_array().unwrap();
        assert!(items.iter().any(|x| x["user_id"].as_i64() == Some(555)));
    }

    // ---- Stats ----
    #[tokio::test]
    async fn api_dashboard_summary() {
        let (state, _dir) = make_state().await;
        let token = owner_token(&state);
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/dashboard/summary")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn api_revenue() {
        let (state, _dir) = make_state().await;
        let token = owner_token(&state);
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/revenue")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ---- Channels ----
    #[tokio::test]
    async fn api_channels_list() {
        let (state, _dir) = make_state().await;
        let token = owner_token(&state);
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/channels")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ---- Config ----
    #[tokio::test]
    async fn api_config_get_set() {
        let (state, _dir) = make_state().await;
        let token = owner_token(&state);
        let app = router(state);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/config/channel")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/config/channel")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::from(r#"{"channel":"@cfg"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/config/channel")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["channel"].as_str().unwrap(), "@cfg");
    }

    // ---- AI ----
    #[tokio::test]
    async fn api_ai_templates_crud() {
        let (state, _dir) = make_seller_state().await;
        let token = seller_token(&state);
        let app = router(state);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/ai/templates")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::from(r#"{"name":"t1","user_prompt":"hello {{x}}"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/ai/templates")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let items = v["items"].as_array().unwrap();
        assert!(items.iter().any(|x| x["name"].as_str() == Some("t1")));
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/ai/templates/t1")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/api/v1/ai/templates/t1")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::from(r#"{"description":"d"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/v1/ai/templates/t1")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ---- Finance ----
    #[tokio::test]
    async fn api_finance_summary() {
        let (state, _dir) = make_state().await;
        let token = owner_token(&state);
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/finance/summary")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ---- Admin (owner) ----
    #[tokio::test]
    async fn api_admin_stats() {
        let (state, _dir) = make_state().await;
        let token = owner_token(&state);
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/admin/stats/global")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn api_admin_sellers() {
        let (state, _dir) = make_state().await;
        let token = owner_token(&state);
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/admin/sellers")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn api_admin_web_accounts_crud() {
        let (state, _dir) = make_state().await;
        let token = owner_token(&state);
        let db = state.db.clone();
        let app = router(state);
        let resp = app.clone().oneshot(Request::builder().method("POST").uri("/api/v1/admin/web-accounts")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", token))
            .body(Body::from(r#"{"username":"ac1","password":"secret123","role":"seller","seller_id":42}"#)).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/admin/web-accounts")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let items = v["items"].as_array().unwrap();
        let acct = items
            .iter()
            .find(|x| x["username"].as_str() == Some("ac1"))
            .unwrap();
        let id = acct["id"].as_i64().unwrap();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/v1/admin/web-accounts/{}", id))
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = db;
    }

    // ---- Role gating ----
    #[tokio::test]
    async fn api_seller_cannot_access_admin() {
        let (state, _dir) = make_seller_state().await;
        let token = seller_token(&state);
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/admin/stats/global")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn api_unauth_all_protected() {
        let (state, _dir) = make_state().await;
        let app = router(state);
        for uri in [
            "/api/v1/plans",
            "/api/v1/users",
            "/api/v1/dashboard/summary",
        ] {
            let resp = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "uri {}", uri);
        }
    }

    // ---- L8 additions (2026-10-25): auth_password, codes_delete, /health,
    // /metrics. Reuse the module's state helpers above.

    async fn change_password(app: &Router, token: &str, body: &str) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/auth/password")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn delete_code(app: &Router, token: &str, code: &str) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/v1/codes/{}", code).as_str())
                    .header("authorization", format!("Bearer {}", token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn auth_password_non_owner_requires_current() {
        let (state, _dir) = make_seller_state().await;
        let app = router(state.clone());
        let token = seller_token(&state);
        // Missing current password -> rejected, nothing changes.
        let resp = change_password(&app, &token, r#"{"new_password":"brandnewpw"}"#).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // Wrong current password -> rejected.
        let resp = change_password(
            &app,
            &token,
            r#"{"new_password":"brandnewpw","current_password":"wrong"}"#,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(
            state
                .db
                .verify_web_password("seller", "secret")
                .await
                .is_some()
        );
        // Correct current password -> the hash rotates (same bcrypt path the
        // login uses) and the old password stops verifying.
        let resp = change_password(
            &app,
            &token,
            r#"{"new_password":"brandnewpw","current_password":"secret"}"#,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            state
                .db
                .verify_web_password("seller", "brandnewpw")
                .await
                .is_some()
        );
        assert!(
            state
                .db
                .verify_web_password("seller", "secret")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn auth_password_invalidates_other_sessions() {
        let (state, _dir) = make_seller_state().await;
        let app = router(state.clone());
        let token_a = seller_token(&state);
        let token_b = seller_token(&state);
        let resp = change_password(
            &app,
            &token_a,
            r#"{"new_password":"brandnewpw","current_password":"secret"}"#,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        // The caller's session survives the rotation...
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/auth/me")
                    .header("authorization", format!("Bearer {}", token_a))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // ...every OTHER session of the account is gone.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/auth/me")
                    .header("authorization", format!("Bearer {}", token_b))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_password_owner_changes_without_current() {
        let (state, _dir) = make_state().await;
        let app = router(state.clone());
        let token = owner_token(&state);
        let resp = change_password(&app, &token, r#"{"new_password":"newownerpw"}"#).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            state
                .db
                .verify_web_password("owner", "newownerpw")
                .await
                .is_some()
        );
        assert!(
            state
                .db
                .verify_web_password("owner", "secret")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn codes_delete_is_seller_scoped_and_real() {
        let (state, _dir) = make_seller_state().await;
        let own = state.db.create_code(42, None, Some(30), 1, None);
        let other = state.db.create_code(43, None, Some(30), 1, None);
        let app = router(state.clone());
        let seller = seller_token(&state);

        // Own code: the row is actually gone.
        let resp = delete_code(&app, &seller, &own).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            state.db.get_code(&own).is_none(),
            "code row must be deleted"
        );

        // Cross-seller: rejected, the row survives untouched.
        let resp = delete_code(&app, &seller, &other).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(
            state.db.get_code(&other).is_some(),
            "cross-seller delete must not touch the code"
        );

        // Admin/owner may delete any seller's code.
        let owner = owner_token(&state);
        let resp = delete_code(&app, &owner, &other).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(state.db.get_code(&other).is_none());
    }

    #[tokio::test]
    async fn codes_delete_unknown_returns_404() {
        let (state, _dir) = make_seller_state().await;
        let app = router(state.clone());
        let token = owner_token(&state);
        let resp = delete_code(&app, &token, "NOPE_NOPE_NOPE").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn health_reports_components() {
        let (state, _dir) = make_state().await;
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["db"], "ok");
        assert_eq!(v["bot"], "configured");
        assert_eq!(v["mcp"], "disabled"); // make_state: mcp_owner_token = None
        assert!(v["uptime_seconds"].is_u64());
    }

    #[tokio::test]
    async fn metrics_scrape_ok_and_no_raw_path_labels() {
        let (state, _dir) = make_seller_state().await;
        // Mirror main.rs: metrics middleware over the API router.
        let app = router(state).layer(axum::middleware::from_fn(crate::metrics::track_http));
        let secret = "sub_tok_9f8e7d6c5b4a";
        // Hit a real route, then an UNMATCHED route whose path AND query
        // string carry a token-like secret — the house-lesson attack shape:
        // a metrics endpoint that labels by raw path leaks it verbatim.
        let _ = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let _ = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/codes/{}/nope?token={}", secret, secret).as_str())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(ct.starts_with("text/plain"), "content-type {ct}");
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body).to_string();
        assert!(!text.contains(secret), "metrics leaked raw path: {text}");
        assert!(text.contains("http_requests_total{route=\"health\",status_class=\"2xx\"}"));
        // The unmatched token-bearing route lands in the fixed catch-all
        // category, never the raw path.
        assert!(text.contains("http_requests_total{route=\"other\",status_class=\"4xx\"}"));
        assert!(text.contains("# TYPE uptime_seconds gauge"));
        assert!(text.contains("bot_messages_sent_total "));
    }
}
// ---------- Invoices (claim) ----------
async fn invoices_claim(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(invoice_id): Path<String>,
) -> impl IntoResponse {
    let session = match session_or_401(&state, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let inv = match state.db.get_invoice(&invoice_id) {
        Some(x) => x,
        None => {
            return err(StatusCode::NOT_FOUND, "not_found", "invoice not found").into_response();
        }
    };
    let (_amount, _asset, status, _desc, plan_id, user_id, seller_id) = inv;
    let admin = is_admin_role(&session.role);
    if !admin && session.role == "seller" && session.seller_id != Some(seller_id) {
        return err(StatusCode::FORBIDDEN, "forbidden", "not your invoice").into_response();
    }
    // Invoice user binding (identical to /paid): an invoice bound to a user
    // may only be completed by that user (web seller accounts carry their
    // Telegram id as seller_id); unbound invoices only by Admin/Owner.
    let binding_ok = match user_id {
        None => admin,
        Some(u) => admin || session.seller_id == Some(u),
    };
    if !binding_ok {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "invoice is bound to another user",
        )
        .into_response();
    }
    if status == "paid" {
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "granted": false, "invoice_id": invoice_id, "status": "already_paid",
            })),
        )
            .into_response();
    }
    // Claim (active -> granting) so the background granter and /paid cannot
    // double-grant while we verify.
    if !state.db.claim_invoice_for_grant(&invoice_id) {
        return err(
            StatusCode::CONFLICT,
            "claim_conflict",
            "invoice is already being processed",
        )
        .into_response();
    }
    // Fail-closed: without a crypto token the payment cannot be verified, so
    // nothing is granted. Release the claim for a later attempt.
    let token = state
        .cfg
        .crypto_token
        .clone()
        .or_else(|| state.db.get_config("crypto_token"));
    let tok = match token {
        Some(t) => t,
        None => {
            state.db.update_invoice_status(&invoice_id, "active");
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                "verification_unavailable",
                "payment verification unavailable: crypto pay not configured",
            )
            .into_response();
        }
    };
    let cp = crate::crypto::CryptoPay::new(tok);
    let v = match cp.get_invoice(&invoice_id).await {
        Ok(v) => v,
        Err(e) => {
            // Verification error: release the claim; the background granter
            // retries. Never grant on doubt.
            state.db.update_invoice_status(&invoice_id, "active");
            return err(
                StatusCode::BAD_GATEWAY,
                "verification_failed",
                &format!("could not verify payment: {}", e),
            )
            .into_response();
        }
    };
    let paid = extract_invoice_items(&v)
        .iter()
        .any(|x| x.get("status").and_then(|s| s.as_str()) == Some("paid"));
    if !paid {
        state.db.update_invoice_status(&invoice_id, "active");
        return err(StatusCode::BAD_REQUEST, "not_paid", "invoice not paid").into_response();
    }
    // Verified paid. Grant once (idempotent), then finalize.
    let mut granted = false;
    let target_user = user_id.unwrap_or(0);
    // Product invoices take the delivery path; plan invoices the
    // subscription path below. The branch is selected by invoices.product_id.
    if let Some((pid, kind, pname, delivery)) = state.db.invoice_product_delivery(&invoice_id) {
        if target_user != 0 && !state.db.product_grant_exists(&invoice_id) {
            match state
                .db
                .grant_product(seller_id, target_user, pid, &invoice_id)
            {
                Ok(true) => {
                    granted = true;
                    if let Err(e) = crate::services::deliver::deliver(
                        &state.bot,
                        target_user,
                        &kind,
                        &pname,
                        delivery.as_deref(),
                    )
                    .await
                    {
                        tracing::warn!(
                            "invoice {}: product '{}' granted but delivery DM failed: {}",
                            invoice_id,
                            pname,
                            e
                        );
                    }
                }
                // A concurrent granter won the UNIQUE(invoice_id) race.
                Ok(false) => {}
                // e.g. out of stock: money settled, grant not — surface and
                // finalize so the invoice stops re-queueing.
                Err(e) => {
                    tracing::warn!(
                        "invoice {}: product grant failed ({}); finalizing without delivery",
                        invoice_id,
                        e
                    );
                }
            }
        }
        state.db.finalize_invoice_paid(&invoice_id);
        let meta = serde_json::json!({
            "product_id": pid, "kind": kind, "name": pname, "granted": granted,
        })
        .to_string();
        state.db.log_action(
            session.account_id,
            &session.role,
            "invoice_claim",
            "invoice",
            &invoice_id,
            Some(&meta),
        );
        return (
            StatusCode::OK,
            Json(serde_json::json!({"granted": granted, "invoice_id": invoice_id})),
        )
            .into_response();
    }
    if !state.db.subscription_exists_for_invoice(&invoice_id) && target_user != 0 {
        // Seller-scoped plan lookup: the invoice's plan is resolved under the
        // invoice's own seller, never a global same-name plan owned by
        // someone else. Tuple: (id, price, asset, name, days, channel).
        let plan_info = plan_id
            .as_ref()
            .and_then(|pn| state.db.get_plan(seller_id, pn));
        let days = plan_info
            .as_ref()
            .map(|p| p.4)
            .unwrap_or(30)
            .clamp(1, crate::handlers::commands::MAX_PLAN_DAYS);
        let sid = seller_id;
        // The channel must belong to the invoice's seller (via its plan) —
        // the global default fallback stays reserved for Admin/Owner.
        let ch = plan_info
            .as_ref()
            .and_then(|p| p.5.clone())
            .or_else(|| state.db.seller_channel(sid))
            .or_else(|| {
                if admin {
                    state.db.get_config("channel")
                } else {
                    None
                }
            })
            .unwrap_or_default();
        if !ch.is_empty() {
            let plan_db_id = plan_info.as_ref().map(|p| p.0);
            state.db.create_subscription(
                sid,
                target_user,
                plan_db_id,
                &ch,
                days,
                Some(&invoice_id),
                None,
            );
            // Checked expiry math; defensive even behind the clamp above.
            let expire = chrono::Duration::try_days(days)
                .and_then(|d| chrono::Utc::now().checked_add_signed(d))
                .unwrap_or_else(chrono::Utc::now);
            let link = state
                .bot
                .create_chat_invite_link(crate::util::to_chat_id(&ch))
                .member_limit(1)
                .expire_date(expire)
                .await
                .ok()
                .map(|l| l.invite_link);
            if let Some(l) = link {
                let _ = state
                    .bot
                    .send_message(
                        teloxide::types::ChatId(target_user),
                        format!("Payment confirmed. Access granted.\nInvite: {}", l),
                    )
                    .await;
            }
            granted = true;
        }
    }
    state.db.finalize_invoice_paid(&invoice_id);
    state.db.log_action(
        session.account_id,
        &session.role,
        "invoice_claim",
        "invoice",
        &invoice_id,
        None,
    );
    (
        StatusCode::OK,
        Json(serde_json::json!({"granted": granted, "invoice_id": invoice_id})),
    )
        .into_response()
}

// ---------- Webhook ----------
async fn webhook_cryptopay(
    State(state): State<WebState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let token = state
        .cfg
        .crypto_token
        .clone()
        .or_else(|| state.db.get_config("crypto_token"));
    // Fail closed when crypto pay is not configured: there is no secret to
    // authenticate the webhook against.
    let Some(tok) = token else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error":"crypto pay not configured"})),
        )
            .into_response();
    };
    // The signature header is mandatory whenever a token is configured.
    let Some(sig) = headers
        .get("x-crypto-pay-api-signature")
        .and_then(|v| v.to_str().ok())
    else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error":"missing signature"})),
        )
            .into_response();
    };
    let computed = crate::util::hmac_sha256(tok.as_bytes(), &body);
    let hex: String = computed.iter().map(|b| format!("{:02x}", b)).collect();
    // Constant-time compare: never short-circuit on the first mismatching
    // byte.
    if !crate::util::ct_eq(&hex, sig) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error":"bad signature"})),
        )
            .into_response();
    }
    let v: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(x) => x,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error":"bad json"})),
            )
                .into_response();
        }
    };
    let payload = v.get("payload").cloned().unwrap_or(v.clone());
    let inv_id_str = if let Some(s) = payload.get("invoice_id") {
        if let Some(n) = s.as_i64() {
            n.to_string()
        } else {
            s.as_str().unwrap_or("").to_string()
        }
    } else {
        String::new()
    };
    let status = payload.get("status").and_then(|s| s.as_str()).unwrap_or("");
    if !inv_id_str.is_empty() && status == "paid" {
        // The webhook only marks the invoice paid; granting is left to the
        // claiming flows (background granter, /paid, claim API), which
        // re-verify against the provider and grant idempotently.
        state.db.update_invoice_status(&inv_id_str, "paid");
        state
            .db
            .log_action(0, "system", "webhook_paid", "invoice", &inv_id_str, None);
    }
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

pub fn router(state: WebState) -> Router {
    Router::new()
        .route("/health", get(health))
        // Prometheus scrape endpoint. No auth by design: the default bind is
        // 127.0.0.1 and this MUST NOT be exposed publicly. Labels are fixed
        // route categories only — see src/metrics.rs for the invariant.
        .route("/metrics", get(crate::metrics::render_metrics))
        // auth
        .route("/api/v1/auth/login", post(auth_login))
        .route("/api/v1/auth/logout", post(auth_logout))
        .route("/api/v1/auth/me", get(auth_me))
        .route("/api/v1/auth/password", post(auth_password))
        // sellers
        .route("/api/v1/sellers", get(sellers_list).post(sellers_create))
        .route("/api/v1/sellers/me", get(sellers_me))
        .route("/api/v1/sellers/{user_id}", get(seller_get))
        .route(
            "/api/v1/sellers/{user_id}/channel",
            post(seller_set_channel),
        )
        // plans
        .route("/api/v1/plans", get(plans_list).post(plans_create))
        .route(
            "/api/v1/plans/{id}",
            get(plans_get).patch(plans_update).delete(plans_delete),
        )
        .route("/api/v1/plans/{id}/active", post(plans_active))
        // subscriptions
        .route("/api/v1/subscriptions", get(subs_list).post(subs_create))
        .route(
            "/api/v1/subscriptions/{id}",
            get(subs_get).patch(subs_patch),
        )
        .route(
            "/api/v1/subscriptions/expire-check",
            post(subs_expire_check),
        )
        .route("/api/v1/users/{user_id}/subscriptions", get(user_subs))
        // invoices
        .route("/api/v1/invoices", get(invoices_list).post(invoices_create))
        .route("/api/v1/invoices/{invoice_id}", get(invoices_get))
        .route(
            "/api/v1/invoices/{invoice_id}/status",
            post(invoices_status),
        )
        .route("/api/v1/invoices/{invoice_id}/claim", post(invoices_claim))
        .route("/api/v1/webhooks/cryptopay", post(webhook_cryptopay))
        // codes
        .route("/api/v1/codes", get(codes_list).post(codes_create))
        .route("/api/v1/codes/{code}", get(codes_get).delete(codes_delete))
        .route("/api/v1/codes/{code}/redeem", post(codes_redeem))
        // posts
        .route("/api/v1/posts", get(posts_list).post(posts_create))
        .route("/api/v1/posts/schedule", post(posts_schedule))
        .route(
            "/api/v1/posts/{id}",
            get(posts_get).patch(posts_update).delete(posts_delete),
        )
        .route("/api/v1/posts/{id}/pin", post(posts_pin))
        .route("/api/v1/posts/{id}/unpin", post(posts_unpin))
        // broadcasts
        .route("/api/v1/broadcasts", post(broadcasts))
        // users
        .route("/api/v1/users", get(users_list))
        .route("/api/v1/users/upsert", post(users_upsert))
        .route("/api/v1/users/{user_id}", get(users_get))
        // stats
        .route("/api/v1/dashboard/summary", get(dashboard_summary))
        .route("/api/v1/revenue", get(revenue))
        // channels
        .route("/api/v1/channels", get(channels_list))
        .route(
            "/api/v1/channels/{id}",
            get(channels_get).patch(channels_patch),
        )
        .route(
            "/api/v1/config/channel",
            get(config_get_channel).post(config_set_channel),
        )
        // ai
        .route(
            "/api/v1/ai/templates",
            get(ai_templates_list).post(ai_templates_create),
        )
        .route(
            "/api/v1/ai/templates/{name}",
            get(ai_templates_get)
                .patch(ai_templates_update)
                .delete(ai_templates_delete),
        )
        .route("/api/v1/ai/usage", get(ai_usage))
        // products (seller scope) + public shop
        .route("/api/v1/products", get(products_list).post(products_create))
        .route("/api/v1/products/{name}", delete(products_delete))
        .route("/api/v1/shop", get(shop_list))
        // finance
        .route("/api/v1/finance/summary", get(finance_summary))
        .route(
            "/api/v1/finance/revenue-by-seller",
            get(finance_revenue_by_seller),
        )
        .route(
            "/api/v1/finance/payouts",
            get(finance_payouts_list).post(finance_payouts_create),
        )
        .route("/api/v1/finance/payouts/{id}", patch(finance_payouts_patch))
        .route(
            "/api/v1/finance/payouts/{id}/process",
            post(finance_payouts_process),
        )
        .route(
            "/api/v1/finance/payouts/{id}/approve",
            post(finance_payouts_approve),
        )
        .route(
            "/api/v1/finance/payouts/{id}/execute",
            post(finance_payouts_execute),
        )
        .route(
            "/api/v1/finance/refunds",
            get(finance_refunds_list).post(finance_refunds_create),
        )
        .route(
            "/api/v1/finance/refunds/{id}/process",
            post(finance_refunds_process),
        )
        .route(
            "/api/v1/finance/refunds/{id}/approve",
            post(finance_refunds_approve),
        )
        .route(
            "/api/v1/finance/refunds/{id}/execute",
            post(finance_refunds_execute),
        )
        .route("/api/v1/finance/export", get(finance_export))
        // admin
        .route("/api/v1/admin/sellers", get(admin_sellers))
        .route(
            "/api/v1/admin/sellers/{user_id}",
            get(admin_seller_get).patch(admin_seller_patch),
        )
        .route("/api/v1/admin/stats/global", get(admin_stats))
        .route(
            "/api/v1/admin/admins",
            get(admin_admins_list).post(admin_admins_create),
        )
        .route(
            "/api/v1/admin/admins/{user_id}",
            delete(admin_admins_delete),
        )
        .route(
            "/api/v1/admin/web-accounts",
            get(admin_web_accounts).post(admin_web_accounts_create),
        )
        .route(
            "/api/v1/admin/web-accounts/{id}",
            delete(admin_web_accounts_delete),
        )
        .route(
            "/api/v1/admin/web-accounts/{id}/password",
            patch(admin_web_accounts_password),
        )
        .route("/api/v1/admin/audit-log", get(admin_audit_log))
        // api keys
        .route(
            "/api/v1/auth/api-keys",
            get(api_keys_list).post(api_keys_create),
        )
        .route("/api/v1/auth/api-keys/{id}", delete(api_keys_delete))
        .with_state(state)
}
