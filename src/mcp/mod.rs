//! MCP server — the fourth thin interface over the domain core (PLAN.md
//! "MCP server (v1.3.x)"): a hand-rolled JSON-RPC 2.0 endpoint at
//! `POST /api/v1/mcp`, zero new dependencies, sharing `WebState` with the
//! web dashboard and REST API.
//!
//! Design invariants:
//! - **Secure default OFF.** With `MCP_OWNER_TOKEN` unset/empty the endpoint
//!   refuses everything with 403 "mcp disabled" — even for a wrong token, so
//!   the error never hints that a valid token could exist.
//! - **Constant-time token compare** (`util::ct_eq`); tokens are never
//!   echoed in any response or log.
//! - **Stateless JSON-RPC 2.0**: single response per request, no
//!   `Mcp-Session-Id`; notifications (no `id`) get HTTP 202 + empty body.
//! - **Tiered tools** (see [`tools`]): Read / Write execute; Sensitive park a
//!   pending approval and return `requires_approval`.
//! - **Audited**: every `tools/call` outcome — ok or err, every tier — lands
//!   in `mcp_audit`.

pub mod tools;

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};
use teloxide::prelude::Requester;

use crate::ai::rate_limit::RateLimiter;
use crate::handlers::registry::{CommandContext, CommandRegistry, CommandSpec};
use crate::roles;
use crate::web::WebState;
use tools::{Principal, Tier};

// JSON-RPC 2.0 error codes (reserved range).
const CODE_PARSE: i64 = -32700;
const CODE_METHOD: i64 = -32601;
const CODE_PARAMS: i64 = -32602;
const CODE_INTERNAL: i64 = -32603;
/// Server-range code for transport-level refusals (disabled / auth / rate).
const CODE_SERVER: i64 = -32000;

/// MCP endpoint budget: same per-principal token-bucket pattern as the AI
/// service (`RateLimiter::new(20)` there); v1 has one principal (the owner).
const RATE_RPM: u32 = 60;

/// Internal per-router state: the shared `WebState` plus a limiter owned by
/// this router instance (so tests and hosts get independent budgets).
#[derive(Clone)]
struct McpState {
    web: WebState,
    limiter: Arc<RateLimiter>,
}

pub fn router(state: WebState) -> Router {
    Router::new()
        .route("/api/v1/mcp", post(mcp_post).get(mcp_get))
        .with_state(McpState {
            web: state,
            limiter: Arc::new(RateLimiter::new(RATE_RPM)),
        })
}

async fn mcp_get() -> Response {
    // MCP v1 is POST-only (JSON-RPC over HTTP); GET is a bare 405.
    StatusCode::METHOD_NOT_ALLOWED.into_response()
}

fn rpc_error(status: StatusCode, id: Value, code: i64, message: &str) -> Response {
    (
        status,
        Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message}
        })),
    )
        .into_response()
}

fn rpc_result(id: Value, result: Value) -> Response {
    (
        StatusCode::OK,
        Json(json!({"jsonrpc": "2.0", "id": id, "result": result})),
    )
        .into_response()
}

/// Bearer-token auth, run before the body is even parsed. Returns the Owner
/// principal or a ready-made error response (boxed: clippy::result_large_err).
fn authenticate(web: &WebState, headers: &HeaderMap) -> Result<Principal, Box<Response>> {
    // Secure default OFF: no configured token means the endpoint does not
    // exist as far as any caller is concerned. Checked BEFORE token
    // comparison so a disabled endpoint behaves identically for every input.
    let Some(expected) = web.cfg.mcp_owner_token.as_deref().filter(|t| !t.is_empty()) else {
        return Err(Box::new(rpc_error(
            StatusCode::FORBIDDEN,
            Value::Null,
            CODE_SERVER,
            "mcp disabled",
        )));
    };
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    // Constant-time compare (the same helper guards webhook signatures).
    // An empty presentation can never match (config rejects empty tokens).
    if presented.is_empty() || !crate::util::ct_eq(presented, expected) {
        return Err(Box::new(rpc_error(
            StatusCode::UNAUTHORIZED,
            Value::Null,
            CODE_SERVER,
            "unauthorized",
        )));
    }
    Ok(Principal::OWNER)
}

/// Rate-limit key: v1 has exactly one principal (the owner), so the role
/// discriminates the single bucket. v2 per-seller tokens will pass
/// `seller_id` here instead.
fn principal_key(principal: &Principal) -> i64 {
    principal.role as i64
}

/// Fixed label for `mcp_tool_calls_total{tier}` — tier names are static
/// literals, never derived from request data (see src/metrics.rs invariant).
fn tier_label(t: Tier) -> &'static str {
    match t {
        Tier::Read => "read",
        Tier::Write => "write",
        Tier::Sensitive => "sensitive",
    }
}

async fn mcp_post(
    State(st): State<McpState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let principal = match authenticate(&st.web, &headers) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    // Per-principal token bucket; the limiter reports the wait in seconds.
    if let Err(retry_secs) = st.limiter.check(principal_key(&principal)) {
        return rpc_error(
            StatusCode::TOO_MANY_REQUESTS,
            Value::Null,
            CODE_SERVER,
            &format!("rate limited, retry in {retry_secs}s"),
        );
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        // Parse error: the id is unknowable, so JSON-RPC mandates null.
        Err(_) => {
            return rpc_error(
                StatusCode::BAD_REQUEST,
                Value::Null,
                CODE_PARSE,
                "parse error",
            );
        }
    };
    dispatch(&st, &principal, req).await
}

/// `tools/call` result envelope: `text` carries the JSON-serialized tool
/// result; `isError` marks execution failures (per MCP, tool errors live in
/// the result, protocol errors in `error`).
fn tool_result_response(id: Value, outcome: Result<Value, String>) -> Response {
    let (payload, is_error) = match outcome {
        Ok(v) => (v, false),
        Err(e) => (json!({"error": e}), true),
    };
    match serde_json::to_string(&payload) {
        Ok(text) => rpc_result(
            id,
            json!({
                "content": [{"type": "text", "text": text}],
                "isError": is_error
            }),
        ),
        // A Value serialization failure is an internal-class fault, not a
        // caller mistake — surface it as -32603 rather than a wrong answer.
        Err(_) => rpc_error(
            StatusCode::OK,
            id,
            CODE_INTERNAL,
            "internal error serializing tool result",
        ),
    }
}

async fn dispatch(st: &McpState, principal: &Principal, req: Value) -> Response {
    // A message without "id" is a notification: JSON-RPC forbids replying, so
    // every notification gets 202 + empty body regardless of method.
    let Some(id) = req.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            // Echo the client's protocol version when it is one we recognize
            // (2024-*/2025-*), else answer with our baseline. Never claim a
            // newer version than the client asked for.
            let requested = params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let version = if requested.starts_with("2025-") || requested.starts_with("2024-") {
                requested
            } else {
                "2025-06-18"
            };
            rpc_result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "telegram-agent", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
        }
        "ping" => rpc_result(id, json!({})),
        "tools/list" => {
            let items: Vec<Value> = tools::registry()
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": t.input_schema,
                        "annotations": t.annotations,
                    })
                })
                .collect();
            rpc_result(id, json!({"tools": items}))
        }
        "tools/call" => {
            let Some(name) = params.get("name").and_then(|v| v.as_str()) else {
                return rpc_error(StatusCode::OK, id, CODE_PARAMS, "missing tool name");
            };
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            if !args.is_object() {
                return rpc_error(
                    StatusCode::OK,
                    id,
                    CODE_PARAMS,
                    "'arguments' must be an object",
                );
            }
            let Some(spec) = tools::find_tool(name) else {
                // Unknown tool is invalid params (-32602), per the design.
                // Metrics: the caller-supplied name is NEVER used as a label;
                // it lands in the fixed "unknown" bucket.
                st.web.db.mcp_audit(
                    name,
                    tools::role_name(principal.role),
                    false,
                    Some("unknown tool"),
                );
                crate::metrics::mcp_tool(None, "unknown", false);
                return rpc_error(StatusCode::OK, id, CODE_PARAMS, "unknown tool");
            };
            // Validate BEFORE anything runs: bad args are -32602 with zero
            // side effects — a sensitive tool must not even park an approval.
            if let Err(msg) = (spec.validate)(&args) {
                st.web.db.mcp_audit(
                    spec.name,
                    tools::role_name(principal.role),
                    false,
                    Some(&format!("invalid args: {msg}")),
                );
                crate::metrics::mcp_tool(Some(spec.name), tier_label(spec.tier), false);
                return rpc_error(StatusCode::OK, id, CODE_PARAMS, &msg);
            }
            let outcome = (spec.handler)(st.web.clone(), principal.clone(), args).await;
            // Every tools/call outcome is audited; the db helper truncates
            // detail, and error strings carry no secrets by construction
            // (gen_codes results only ever travel to the owner chat).
            match &outcome {
                Ok(v) => {
                    let detail = if v.get("requires_approval").is_some() {
                        format!(
                            "parked approval #{}",
                            v.get("approval_id").and_then(|x| x.as_i64()).unwrap_or(0)
                        )
                    } else {
                        "ok".to_string()
                    };
                    st.web.db.mcp_audit(
                        spec.name,
                        tools::role_name(principal.role),
                        true,
                        Some(&detail),
                    );
                }
                Err(e) => {
                    st.web.db.mcp_audit(
                        spec.name,
                        tools::role_name(principal.role),
                        false,
                        Some(e),
                    );
                }
            }
            // Metrics hook: every tools/call outcome (parked approvals count
            // as ok=true — the call itself succeeded and returned a result).
            crate::metrics::mcp_tool(Some(spec.name), tier_label(spec.tier), outcome.is_ok());
            tool_result_response(id, outcome)
        }
        "" => rpc_error(StatusCode::OK, id, CODE_METHOD, "missing method"),
        _ => rpc_error(StatusCode::OK, id, CODE_METHOD, "method not found"),
    }
}

// ---- Owner approval commands (/approvals, /approve <id>, /deny <id>) ----
//
// These run through the runtime command registry (owner-only via min_role).
// An approval is decided BEFORE it executes (an atomic pending->decided flip
// in db::mcp_decide_approval), so a double-approve can never execute twice,
// and execution goes through the same tool layer the MCP call path uses.

/// Age label for the /approvals listing.
fn age_of(created_at: &str) -> String {
    let Ok(created) = chrono::DateTime::parse_from_rfc3339(created_at) else {
        return "unknown age".into();
    };
    let secs = chrono::Utc::now()
        .signed_duration_since(created)
        .num_seconds()
        .max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// /approvals — list pending approvals (oldest first).
pub fn cmd_approvals(state: &WebState) -> String {
    let rows = state.db.mcp_list_approvals(Some("pending"), 20);
    if rows.is_empty() {
        return "No pending MCP approvals.".into();
    }
    let mut out = String::from("Pending MCP approvals:\n");
    for row in &rows {
        out.push_str(&format!("#{} {} ({} old)\n", row.0, row.1, age_of(&row.4)));
    }
    out.push_str("Approve: /approve <id> · Deny: /deny <id>");
    out
}

/// /approve <id> — decide-then-execute an approved sensitive tool. The return
/// value is the owner-chat message; for gen_codes it contains the FULL codes,
/// which is exactly why it must only ever travel to this chat.
pub async fn cmd_approve(state: &WebState, args: &str) -> String {
    // split_whitespace already ignores surrounding whitespace, so no trim.
    let Some(id) = args.split_whitespace().next().and_then(|s| s.parse().ok()) else {
        return "Usage: /approve <id>".into();
    };
    let Some(row) = state.db.mcp_get_approval(id) else {
        return format!("Approval #{id} not found.");
    };
    if row.3 != "pending" {
        return format!("Approval #{id} is already {}.", row.3);
    }
    // Expired rows are decided to 'expired' (the decide only succeeds from
    // 'pending', so this doubles as the atomic claim).
    let now = chrono::Utc::now().to_rfc3339();
    if row.5 < now {
        state.db.mcp_decide_approval(id, "expired");
        state.db.mcp_audit(
            &row.1,
            "owner",
            false,
            Some(&format!("approval #{id} expired")),
        );
        return format!("Approval #{id} expired.");
    }
    if !state.db.mcp_decide_approval(id, "approved") {
        // Lost a concurrent decide (double-approve): nothing executes.
        state.db.mcp_audit(
            &row.1,
            "owner",
            false,
            Some(&format!("approval #{id} already decided")),
        );
        return format!("Approval #{id} is already decided.");
    }
    let tool_args: Value = serde_json::from_str(&row.2).unwrap_or(json!({}));
    // The SAME tool layer the MCP call path would have used (dispatch by name
    // with the stored args; the validator re-runs inside).
    match tools::execute_sensitive(state.clone(), &Principal::OWNER, &row.1, tool_args).await {
        Ok(v) => {
            state.db.mcp_mark_executed(id);
            state.db.mcp_audit(
                &row.1,
                "owner",
                true,
                Some(&format!("approved #{id} executed")),
            );
            let pretty = serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string());
            format!("Approved #{} ({}):\n{}", id, row.1, pretty)
        }
        Err(e) => {
            // Decided but not executed: the owner sees the failure and can
            // re-issue the tool call; the row never claims execution.
            state.db.mcp_audit(
                &row.1,
                "owner",
                false,
                Some(&format!("approved #{id} execution failed: {e}")),
            );
            format!("Approval #{} ({}): execution failed: {}", id, row.1, e)
        }
    }
}

/// /deny <id> — mark a pending approval denied. Denials never execute.
pub async fn cmd_deny(state: &WebState, args: &str) -> String {
    // split_whitespace already ignores surrounding whitespace, so no trim.
    let Some(id) = args.split_whitespace().next().and_then(|s| s.parse().ok()) else {
        return "Usage: /deny <id>".into();
    };
    let tool = state
        .db
        .mcp_get_approval(id)
        .map(|r| r.1)
        .unwrap_or_else(|| "unknown".into());
    if state.db.mcp_decide_approval(id, "denied") {
        state
            .db
            .mcp_audit(&tool, "owner", true, Some(&format!("denied #{id}")));
        format!("Approval #{id} denied.")
    } else {
        match state.db.mcp_get_approval(id) {
            Some(row) => format!("Approval #{id} is already {}.", row.3),
            None => format!("Approval #{id} not found."),
        }
    }
}

/// Register the three owner commands into the runtime registry at startup.
/// `state` is captured so an approval executes with the same WebState the MCP
/// endpoint uses. The registry itself is the role gate (min_role Owner): the
/// dispatcher silently ignores callers below the spec's min_role.
pub fn register_commands(reg: &mut CommandRegistry, state: WebState) {
    // Replies go back to the chat the command came from; a failed send must
    // never crash the dispatcher, so send errors are swallowed (the approval
    // row + audit trail still record what happened).
    if let Err(e) = reg.register(CommandSpec {
        name: "approvals".into(),
        description: "List pending MCP tool approvals (owner only)".into(),
        min_role: roles::Role::Owner,
        handler: {
            let st = state.clone();
            Arc::new(move |ctx: CommandContext| {
                let st = st.clone();
                Box::pin(async move {
                    let reply = cmd_approvals(&st);
                    let chat = ctx.msg.chat.id;
                    let _ = ctx.bot.send_message(chat, reply).await;
                    Ok(())
                })
            })
        },
    }) {
        tracing::error!("failed to register /approvals: {e}");
    }
    if let Err(e) = reg.register(CommandSpec {
        name: "approve".into(),
        description: "Approve a pending MCP tool call: /approve <id> (owner only)".into(),
        min_role: roles::Role::Owner,
        handler: {
            let st = state.clone();
            Arc::new(move |ctx: CommandContext| {
                let st = st.clone();
                Box::pin(async move {
                    let reply = cmd_approve(&st, &ctx.args).await;
                    let chat = ctx.msg.chat.id;
                    let _ = ctx.bot.send_message(chat, reply).await;
                    Ok(())
                })
            })
        },
    }) {
        tracing::error!("failed to register /approve: {e}");
    }
    if let Err(e) = reg.register(CommandSpec {
        name: "deny".into(),
        description: "Deny a pending MCP tool call: /deny <id> (owner only)".into(),
        min_role: roles::Role::Owner,
        handler: {
            let st = state.clone();
            Arc::new(move |ctx: CommandContext| {
                let st = st.clone();
                Box::pin(async move {
                    let reply = cmd_deny(&st, &ctx.args).await;
                    let chat = ctx.msg.chat.id;
                    let _ = ctx.bot.send_message(chat, reply).await;
                    Ok(())
                })
            })
        },
    }) {
        tracing::error!("failed to register /deny: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    const TOKEN: &str = "test-owner-token";

    /// In-process MCP state: temp-dir Db, placeholder bot (never contacted —
    /// tests only hit tools that don't send, or assert the send error path),
    /// and an optional owner token.
    fn mcp_state(token: Option<&str>) -> (WebState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db").to_str().unwrap().to_string();
        let db = Db::new(&db_path).unwrap();
        let cfg = Config {
            bot_token: "0:setup-placeholder".to_string(),
            owner_id: 1,
            db_path,
            default_channel: None,
            crypto_token: None,
            ai_enabled: false,
            ai_provider: "openai".to_string(),
            ai_base_url: "https://api.openai.com/v1".to_string(),
            ai_api_key: None,
            ai_model: "gpt-4o-mini".to_string(),
            ai_daily_token_budget: None,
            mcp_owner_token: token.map(str::to_string),
        };
        let state = WebState {
            db: Arc::new(db),
            cfg,
            bot: teloxide::Bot::new("0:setup-placeholder"),
            login_attempts: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            base_path: String::new(),
        };
        (state, dir)
    }

    fn rpc(id: i64, method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }

    async fn send(app: &Router, token: Option<&str>, body: String) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/api/v1/mcp")
            .header("content-type", "application/json");
        if let Some(t) = token {
            builder = builder.header("authorization", format!("Bearer {t}"));
        }
        let resp = app
            .clone()
            .oneshot(builder.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let v = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, v)
    }

    async fn call(app: &Router, body: Value) -> (StatusCode, Value) {
        send(app, Some(TOKEN), body.to_string()).await
    }

    /// Pull the JSON-serialized tool result text back out of the envelope.
    fn result_text(v: &Value) -> Value {
        let text = v["result"]["content"][0]["text"].as_str().unwrap_or("");
        serde_json::from_str(text).unwrap_or(Value::Null)
    }

    fn err_code(v: &Value) -> i64 {
        v["error"]["code"].as_i64().unwrap_or(0)
    }

    // ---- Auth ----

    #[tokio::test]
    async fn disabled_endpoint_returns_403_even_for_a_wrong_token() {
        let (state, _dir) = mcp_state(None);
        let app = router(state.clone());
        // Secure default OFF: any bearer (here: a wrong one) must get the
        // same "mcp disabled" refusal, with no echo of what was presented.
        let (status, v) = send(
            &app,
            Some("whatever"),
            rpc(1, "ping", json!({})).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(err_code(&v), CODE_SERVER);
        assert_eq!(v["error"]["message"], "mcp disabled");
        assert!(!v.to_string().contains("whatever"));
        // No Authorization header at all behaves identically.
        let (status, v) = send(&app, None, rpc(2, "ping", json!({})).to_string()).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["message"], "mcp disabled");
    }

    #[tokio::test]
    async fn wrong_token_returns_401_without_echoing() {
        let (state, _dir) = mcp_state(Some(TOKEN));
        let app = router(state.clone());
        let (status, v) = send(&app, Some("nope"), rpc(1, "ping", json!({})).to_string()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(!v.to_string().contains(TOKEN));
        assert!(!v.to_string().contains("nope"));
    }

    #[tokio::test]
    async fn get_is_method_not_allowed() {
        let (state, _dir) = mcp_state(Some(TOKEN));
        let app = router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/mcp")
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    // ---- Protocol ----

    #[tokio::test]
    async fn initialize_echoes_known_version_and_downgrades_unknown() {
        let (state, _dir) = mcp_state(Some(TOKEN));
        let app = router(state.clone());
        for (requested, expect) in [
            ("2025-03-26", "2025-03-26"),
            ("2025-06-18", "2025-06-18"),
            ("2024-11-05", "2024-11-05"),
            ("1999-01-01", "2025-06-18"),
            ("garbage", "2025-06-18"),
        ] {
            let (_, v) = call(
                &app,
                rpc(
                    1,
                    "initialize",
                    json!({"protocolVersion": requested, "capabilities": {}}),
                ),
            )
            .await;
            assert_eq!(
                v["result"]["protocolVersion"], expect,
                "requested {requested}"
            );
            assert_eq!(v["result"]["capabilities"]["tools"]["listChanged"], false);
            assert_eq!(v["result"]["serverInfo"]["name"], "telegram-agent");
        }
    }

    #[tokio::test]
    async fn initialized_notification_gets_202_empty_body() {
        let (state, _dir) = mcp_state(Some(TOKEN));
        let app = router(state.clone());
        // No "id" field: a notification. JSON-RPC forbids a response body.
        let body = json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string();
        let (status, v) = send(&app, Some(TOKEN), body).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(v, Value::Null);
    }

    #[tokio::test]
    async fn ping_returns_empty_result() {
        let (state, _dir) = mcp_state(Some(TOKEN));
        let app = router(state.clone());
        let (status, v) = call(&app, rpc(7, "ping", json!({}))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["result"], json!({}));
        assert_eq!(v["id"], 7);
    }

    #[tokio::test]
    async fn protocol_errors_use_the_right_codes() {
        let (state, _dir) = mcp_state(Some(TOKEN));
        let app = router(state.clone());
        // parse error
        let (status, v) = send(&app, Some(TOKEN), "{not json".to_string()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(err_code(&v), CODE_PARSE);
        assert_eq!(v["id"], Value::Null);
        // unknown method
        let (_, v) = call(&app, rpc(1, "no/such", json!({}))).await;
        assert_eq!(err_code(&v), CODE_METHOD);
        // missing method
        let (_, v) = call(&app, json!({"jsonrpc": "2.0", "id": 2})).await;
        assert_eq!(err_code(&v), CODE_METHOD);
        // missing tool name / non-object arguments
        let (_, v) = call(&app, rpc(3, "tools/call", json!({}))).await;
        assert_eq!(err_code(&v), CODE_PARAMS);
        let (_, v) = call(
            &app,
            rpc(
                4,
                "tools/call",
                json!({"name": "list_posts", "arguments": [1]}),
            ),
        )
        .await;
        assert_eq!(err_code(&v), CODE_PARAMS);
        // unknown tool
        let (_, v) = call(
            &app,
            rpc(5, "tools/call", json!({"name": "nope", "arguments": {}})),
        )
        .await;
        assert_eq!(err_code(&v), CODE_PARAMS);
        assert_eq!(v["error"]["message"], "unknown tool");
    }

    #[tokio::test]
    async fn tools_list_exposes_all_tiers_with_annotations() {
        let (state, _dir) = mcp_state(Some(TOKEN));
        let app = router(state.clone());
        let (_, v) = call(&app, rpc(1, "tools/list", json!({}))).await;
        let tools = v["result"]["tools"].as_array().unwrap();
        let mut names = std::collections::BTreeSet::new();
        for t in tools {
            names.insert(t["name"].as_str().unwrap().to_string());
            let annotations = &t["annotations"];
            match t["name"].as_str().unwrap() {
                "list_channels" | "list_plans" | "list_posts" | "subs_report" | "list_codes" => {
                    assert_eq!(annotations["readOnlyHint"], true, "{}", t["name"]);
                }
                "publish_post" | "edit_post" => {
                    assert_eq!(annotations["readOnlyHint"], false, "{}", t["name"]);
                }
                "gen_codes" | "delete_post" | "broadcast" => {
                    assert_eq!(annotations["readOnlyHint"], false, "{}", t["name"]);
                    assert_eq!(annotations["destructiveHint"], true, "{}", t["name"]);
                    assert_eq!(annotations["x-approval-required"], true, "{}", t["name"]);
                }
                other => panic!("unexpected tool {other}"),
            }
        }
        for expected in [
            "list_channels",
            "list_plans",
            "list_posts",
            "subs_report",
            "list_codes",
            "publish_post",
            "edit_post",
            "gen_codes",
            "delete_post",
            "broadcast",
        ] {
            assert!(names.contains(expected), "missing tool {expected}");
        }
        // gen_codes schema: integer bounds present, no secret-shaped defaults.
        let gen_spec = tools.iter().find(|t| t["name"] == "gen_codes").unwrap();
        assert_eq!(
            gen_spec["inputSchema"]["properties"]["count"]["maximum"],
            50
        );
        assert!(
            gen_spec["inputSchema"]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("count"))
        );
        for (_k, prop) in gen_spec["inputSchema"]["properties"].as_object().unwrap() {
            assert!(
                prop.get("default").map(|d| !d.is_string()).unwrap_or(true),
                "gen_codes schema must not carry string defaults"
            );
        }
    }

    // ---- Read tools ----

    #[tokio::test]
    async fn list_codes_are_masked_on_the_read_path() {
        let (state, _dir) = mcp_state(Some(TOKEN));
        state.db.set_config("channel", "@chan");
        let full = state.db.create_code(1, None, Some(30), 1, None);
        let app = router(state.clone());
        let (_, v) = call(
            &app,
            rpc(
                1,
                "tools/call",
                json!({"name": "list_codes", "arguments": {"limit": 10}}),
            ),
        )
        .await;
        let text = serde_json::to_string(&v).unwrap();
        let expected_mask = format!("{}…", &full[..4]);
        assert!(
            text.contains(&expected_mask),
            "masked form must appear: {text}"
        );
        // The full secret must not appear anywhere in the response.
        assert!(!text.contains(&full), "full code leaked: {text}");
        let result = result_text(&v);
        assert_eq!(result["items"][0]["code"], expected_mask);
        assert_eq!(result["items"][0]["max_uses"], 1);
    }

    #[tokio::test]
    async fn subs_report_masks_pii_by_default_and_flags_it_when_included() {
        let (state, _dir) = mcp_state(Some(TOKEN));
        state
            .db
            .create_subscription(1, 4242, None, "@chan", 30, None, None);
        let app = router(state.clone());
        // Default: aggregates only.
        let (_, v) = call(
            &app,
            rpc(
                1,
                "tools/call",
                json!({"name": "subs_report", "arguments": {}}),
            ),
        )
        .await;
        let body = serde_json::to_string(&v).unwrap();
        assert!(!body.contains("4242"), "user id leaked by default: {body}");
        assert_eq!(result_text(&v)["by_status"]["active"], 1);
        assert_eq!(result_text(&v)["contains_pii"], false);
        // Opt-in: rows carry user ids and the contains_pii flag.
        let (_, v) = call(
            &app,
            rpc(
                2,
                "tools/call",
                json!({"name": "subs_report", "arguments": {"include_user_ids": true}}),
            ),
        )
        .await;
        let result = result_text(&v);
        assert_eq!(result["contains_pii"], true);
        assert_eq!(result["rows"][0]["user_id"], 4242);
    }

    #[tokio::test]
    async fn list_channels_and_plans_reflect_the_db() {
        let (state, _dir) = mcp_state(Some(TOKEN));
        state.db.set_config("channel", "@default");
        state.db.upsert_seller(5, Some("s"), Some("@seller5"));
        state
            .db
            .add_plan(5, "basic", "10", "USDT", 30, Some("@seller5"));
        let app = router(state.clone());
        let (_, v) = call(
            &app,
            rpc(
                1,
                "tools/call",
                json!({"name": "list_channels", "arguments": {}}),
            ),
        )
        .await;
        let result = result_text(&v);
        assert_eq!(result["default"], "@default");
        assert_eq!(result["sellers"][0]["seller_id"], 5);
        assert_eq!(result["sellers"][0]["channel"], "@seller5");
        let (_, v) = call(
            &app,
            rpc(
                2,
                "tools/call",
                json!({"name": "list_plans", "arguments": {}}),
            ),
        )
        .await;
        let result = result_text(&v);
        assert_eq!(result["total"], 1);
        assert_eq!(result["items"][0]["name"], "basic");
        assert_eq!(result["items"][0]["active"], true);
    }

    // ---- Write tools ----

    #[tokio::test]
    async fn publish_post_rejects_missing_text_as_invalid_params() {
        let (state, _dir) = mcp_state(Some(TOKEN));
        state.db.set_config("channel", "@chan");
        let app = router(state.clone());
        let (_, v) = call(
            &app,
            rpc(
                1,
                "tools/call",
                json!({"name": "publish_post", "arguments": {}}),
            ),
        )
        .await;
        assert_eq!(err_code(&v), CODE_PARAMS);
        // Whitespace-only text is equally invalid.
        let (_, v) = call(
            &app,
            rpc(
                2,
                "tools/call",
                json!({"name": "publish_post", "arguments": {"text": "   "}}),
            ),
        )
        .await;
        assert_eq!(err_code(&v), CODE_PARAMS);
        // Nothing was logged.
        assert_eq!(state.db.count_posts(None), 0);
    }

    #[tokio::test]
    async fn edit_post_updates_the_stored_record() {
        let (state, _dir) = mcp_state(Some(TOKEN));
        let _ = state.db.log_post(0, "@chan", 11, "text", Some("before"));
        let app = router(state.clone());
        let (_, v) = call(
            &app,
            rpc(
                1,
                "tools/call",
                json!({"name": "edit_post", "arguments": {"post_id": 1, "text": "after"}}),
            ),
        )
        .await;
        let result = result_text(&v);
        assert_eq!(result["ok"], true);
        let posts = state.db.list_posts(None, 10);
        assert_eq!(posts[0].5.as_deref(), Some("after"));
    }

    // ---- Sensitive tier: the approval gate ----

    #[tokio::test]
    async fn gen_codes_parks_an_approval_without_side_effects() {
        let (state, _dir) = mcp_state(Some(TOKEN));
        let before = state.db.count_codes(None);
        let app = router(state.clone());
        let (_, v) = call(
            &app,
            rpc(
                1,
                "tools/call",
                json!({"name": "gen_codes", "arguments": {"count": 3, "days": 30}}),
            ),
        )
        .await;
        let result = result_text(&v);
        assert_eq!(result["requires_approval"], true);
        assert!(result["approval_id"].as_i64().unwrap() > 0);
        assert_eq!(result["expires_at"].as_str().is_some(), true);
        let how = result["how"].as_str().unwrap();
        assert!(how.contains("/approve"), "{how}");
        // No codes exist yet: the call path never executes.
        let row = state
            .db
            .mcp_get_approval(result["approval_id"].as_i64().unwrap())
            .unwrap();
        assert_eq!(row.1, "gen_codes");
        assert_eq!(row.3, "pending");
        let after = state.db.count_codes(None);
        assert_eq!(before, after, "gen_codes call path must not create codes");
    }

    #[tokio::test]
    async fn bad_args_on_a_sensitive_tool_fail_without_parking() {
        let (state, _dir) = mcp_state(Some(TOKEN));
        let app = router(state.clone());
        let (_, v) = call(
            &app,
            rpc(
                1,
                "tools/call",
                json!({"name": "gen_codes", "arguments": {"count": 999}}),
            ),
        )
        .await;
        assert_eq!(err_code(&v), CODE_PARAMS);
        assert!(state.db.mcp_list_approvals(Some("pending"), 10).is_empty());
    }

    #[tokio::test]
    async fn approved_gen_codes_executes_and_delivers_codes_to_the_owner_chat_only() {
        let (state, _dir) = mcp_state(Some(TOKEN));
        let before = state.db.count_codes(None);
        let app = router(state.clone());
        let (_, v) = call(
            &app,
            rpc(
                1,
                "tools/call",
                json!({"name": "gen_codes", "arguments": {"count": 2}}),
            ),
        )
        .await;
        let id = result_text(&v)["approval_id"].as_i64().unwrap();
        // The MCP response predates the codes, so it cannot contain them...
        assert!(!serde_json::to_string(&v).unwrap().contains("\"codes\""));
        // ...the owner's approve reply is where the FULL codes land.
        let reply = cmd_approve(&state, &id.to_string()).await;
        assert!(
            reply.contains("codes"),
            "owner reply must carry the codes: {reply}"
        );
        assert_eq!(state.db.count_codes(None), before + 2);
        let row = state.db.mcp_get_approval(id).unwrap();
        assert_eq!(row.3, "approved");
        assert!(row.7.is_some(), "row must be marked executed");
        // The approval lifecycle is audited.
        let audit = state.db.mcp_list_audit(10);
        assert!(
            audit.iter().any(|a| a.1 == "gen_codes" && a.3),
            "audit row missing"
        );
    }

    #[tokio::test]
    async fn denied_approval_never_executes() {
        let (state, _dir) = mcp_state(Some(TOKEN));
        let before = state.db.count_codes(None);
        let app = router(state.clone());
        let (_, v) = call(
            &app,
            rpc(
                1,
                "tools/call",
                json!({"name": "gen_codes", "arguments": {"count": 2}}),
            ),
        )
        .await;
        let id = result_text(&v)["approval_id"].as_i64().unwrap();
        let reply = cmd_deny(&state, &id.to_string()).await;
        assert!(reply.contains("denied"), "{reply}");
        assert_eq!(state.db.count_codes(None), before);
        let row = state.db.mcp_get_approval(id).unwrap();
        assert_eq!(row.3, "denied");
        assert!(row.7.is_none());
    }

    #[tokio::test]
    async fn expired_approval_is_rejected() {
        let (state, _dir) = mcp_state(Some(TOKEN));
        let before = state.db.count_codes(None);
        // Park a row already past its expiry (the insert helper takes the
        // timestamp, so no sleeping in tests).
        let id = state.db.mcp_create_approval(
            "gen_codes",
            r#"{"count":2}"#,
            "2020-01-01T00:00:00+00:00",
        );
        let reply = cmd_approve(&state, &id.to_string()).await;
        assert!(reply.contains("expired"), "{reply}");
        assert_eq!(state.db.count_codes(None), before);
        assert_eq!(state.db.mcp_get_approval(id).unwrap().3, "expired");
    }

    #[tokio::test]
    async fn double_approve_executes_only_once() {
        let (state, _dir) = mcp_state(Some(TOKEN));
        let app = router(state.clone());
        let (_, v) = call(
            &app,
            rpc(
                1,
                "tools/call",
                json!({"name": "gen_codes", "arguments": {"count": 1}}),
            ),
        )
        .await;
        let id = result_text(&v)["approval_id"].as_i64().unwrap();
        let first = cmd_approve(&state, &id.to_string()).await;
        assert!(first.contains("codes"), "{first}");
        let after_first = state.db.count_codes(None);
        let second = cmd_approve(&state, &id.to_string()).await;
        // The status gate reports the decision (the "already decided" race
        // branch only fires on a concurrent decide); either way nothing runs.
        assert!(second.contains("already"), "{second}");
        assert_eq!(
            state.db.count_codes(None),
            after_first,
            "second approve must not execute"
        );
    }

    #[tokio::test]
    async fn approvals_listing_shows_pending_rows() {
        let (state, _dir) = mcp_state(Some(TOKEN));
        let app = router(state.clone());
        let _ = call(
            &app,
            rpc(
                1,
                "tools/call",
                json!({"name": "delete_post", "arguments": {"post_id": 9}}),
            ),
        )
        .await;
        let listing = cmd_approvals(&state);
        assert!(listing.contains("delete_post"), "{listing}");
        assert!(listing.contains("#1"), "{listing}");
        // An expired pending disappears from the pending list (lazy purge).
        let _ = state.db.mcp_create_approval(
            "broadcast",
            r#"{"text":"x"}"#,
            "2020-01-01T00:00:00+00:00",
        );
        let listing = cmd_approvals(&state);
        assert!(listing.contains("delete_post"));
        assert!(
            !listing.contains("broadcast"),
            "expired row must be purged: {listing}"
        );
    }

    // ---- Audit ----

    #[tokio::test]
    async fn every_tools_call_outcome_is_audited() {
        let (state, _dir) = mcp_state(Some(TOKEN));
        state.db.set_config("channel", "@chan");
        let app = router(state.clone());
        // ok (read), err (invalid args on a write), err (unknown tool).
        let _ = call(
            &app,
            rpc(
                1,
                "tools/call",
                json!({"name": "list_plans", "arguments": {}}),
            ),
        )
        .await;
        let _ = call(
            &app,
            rpc(
                2,
                "tools/call",
                json!({"name": "publish_post", "arguments": {}}),
            ),
        )
        .await;
        let _ = call(
            &app,
            rpc(3, "tools/call", json!({"name": "nope", "arguments": {}})),
        )
        .await;
        let audit = state.db.mcp_list_audit(10);
        assert!(audit.iter().any(|a| a.1 == "list_plans" && a.3));
        assert!(audit.iter().any(|a| a.1 == "publish_post" && !a.3));
        assert!(audit.iter().any(|a| a.1 == "nope" && !a.3));
        // All rows are owner-role.
        assert!(audit.iter().all(|a| a.2 == "owner"));
    }

    // ---- Rate limit ----

    #[tokio::test]
    async fn request_burst_is_rate_limited() {
        let (state, _dir) = mcp_state(Some(TOKEN));
        let app = router(state.clone());
        let body = rpc(1, "tools/list", json!({})).to_string();
        // The bucket holds 60 tokens; hammer well past capacity + refill and
        // expect at least one 429 (in-process calls are sub-millisecond).
        let mut limited = 0;
        for _ in 0..100 {
            let (status, v) = send(&app, Some(TOKEN), body.clone()).await;
            if status == StatusCode::TOO_MANY_REQUESTS {
                limited += 1;
                assert_eq!(err_code(&v), CODE_SERVER);
                break;
            }
        }
        assert!(limited > 0, "expected the burst to trip the rate limiter");
    }
}
