//! MCP tool registry: the tool table, its three permission tiers, and the
//! handlers behind them.
//!
//! The tier model is the permission system:
//! - *Read* tools are direct, annotated `readOnlyHint: true`, and mask any
//!   secret-bearing field (codes) or PII (subscriber user ids) by default.
//! - *Write* tools execute immediately (mirroring the web API's behavior) and
//!   are audited like everything else.
//! - *Sensitive* tools NEVER execute on the MCP call path. Calling one parks
//!   a pending approval row and returns `requires_approval`; the actual work
//!   runs only from the owner's `/approve` via [`execute_sensitive`], which
//!   goes through the same validate + execute functions the call path would
//!   have used. Generated codes therefore reach the owner's Telegram chat and
//!   nothing else — never the AI transcript, never `mcp_audit`.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use serde_json::{Value, json};
use teloxide::prelude::Requester;

use crate::roles;
use crate::web::WebState;

/// How long a parked approval stays actionable. The owner must decide within
/// this window or the row expires (purged lazily on list).
const APPROVAL_TTL_SECS: i64 = 15 * 60;

/// Upper bound for `gen_codes` count / list-tool limits — matches the bot's
/// `/gencodes` cap so no interface can mint unbounded code bursts.
pub const MAX_CODES: i64 = 50;
pub const DEFAULT_LIST_LIMIT: i64 = 20;
pub const MAX_LIST_LIMIT: i64 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Read,
    Write,
    Sensitive,
}

/// Who is calling. v1 mints exactly one principal (the static owner bearer
/// token), but every handler threads it so per-seller tokens (v2) are
/// additive: handlers scope their queries by `seller_id`, where `None` means
/// the unscoped owner view.
#[derive(Debug, Clone)]
pub struct Principal {
    pub role: roles::Role,
    pub seller_id: Option<i64>,
}

impl Principal {
    pub const OWNER: Principal = Principal {
        role: roles::Role::Owner,
        seller_id: None,
    };
}

/// Lowercase role name for audit rows.
pub fn role_name(role: roles::Role) -> &'static str {
    match role {
        roles::Role::User => "user",
        roles::Role::Seller => "seller",
        roles::Role::Admin => "admin",
        roles::Role::Owner => "owner",
    }
}

pub type HandlerFuture = Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>;
/// Handlers take owned clones (`WebState`/`Principal` are cheap-Clone) so the
/// boxed futures stay `'static`; an Arc'd closure (not a fn pointer) lets the
/// registry carry the generic `h()` wrapper without lifetime plumbing.
pub type Handler = Arc<dyn Fn(WebState, Principal, Value) -> HandlerFuture + Send + Sync>;
pub type Validator = fn(&Value) -> Result<(), String>;

pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub tier: Tier,
    pub input_schema: Value,
    pub annotations: Value,
    /// Pure argument check, run by the dispatcher BEFORE any handler runs so
    /// bad arguments fail with -32602 and zero side effects — including for
    /// sensitive tools, which must not park an approval for garbage args.
    pub validate: Validator,
    pub handler: Handler,
}

/// The static tool table. Built once via `OnceLock` (the schemas are runtime
/// JSON values, so a plain `static` cannot hold them).
pub fn registry() -> &'static [ToolSpec] {
    static REG: OnceLock<Vec<ToolSpec>> = OnceLock::new();
    REG.get_or_init(|| {
        vec![
            // ---- Tier: Read ----
            ToolSpec {
                name: "list_channels",
                description: "List the default target channel and each seller's channel.",
                tier: Tier::Read,
                input_schema: json!({"type": "object", "properties": {}}),
                annotations: json!({"readOnlyHint": true}),
                validate: validate_empty,
                handler: h(list_channels),
            },
            ToolSpec {
                name: "list_plans",
                description: "List subscription plans.",
                tier: Tier::Read,
                input_schema: json!({"type": "object", "properties": {}}),
                annotations: json!({"readOnlyHint": true}),
                validate: validate_empty,
                handler: h(list_plans),
            },
            ToolSpec {
                name: "list_posts",
                description: "List recent posts (stored records, newest first).",
                tier: Tier::Read,
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "limit": {"type": "integer", "minimum": 1, "maximum": MAX_LIST_LIMIT,
                                  "description": format!("How many posts to return (default {DEFAULT_LIST_LIMIT}).")}
                    }
                }),
                annotations: json!({"readOnlyHint": true}),
                validate: validate_limit,
                handler: h(list_posts),
            },
            ToolSpec {
                name: "subs_report",
                description: "Subscription report: counts by status. User ids are masked \
                              unless include_user_ids=true (the response is then flagged \
                              contains_pii).",
                tier: Tier::Read,
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "include_user_ids": {"type": "boolean", "default": false,
                                             "description": "Include subscriber user ids (PII)."}
                    }
                }),
                annotations: json!({"readOnlyHint": true}),
                validate: validate_subs_report,
                handler: h(subs_report),
            },
            ToolSpec {
                name: "list_codes",
                description: "List recent activation codes, MASKED (first 4 chars). Full \
                              codes are only ever shown to the owner in the bot after an \
                              approved gen_codes.",
                tier: Tier::Read,
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "limit": {"type": "integer", "minimum": 1, "maximum": MAX_LIST_LIMIT,
                                  "description": format!("How many codes to return (default {DEFAULT_LIST_LIMIT}).")}
                    }
                }),
                annotations: json!({"readOnlyHint": true}),
                validate: validate_limit,
                handler: h(list_codes),
            },
            // ---- Tier: Write ----
            ToolSpec {
                name: "publish_post",
                description: "Publish a text post to a channel (default channel unless \
                              `channel` is given). Executes immediately.",
                tier: Tier::Write,
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "channel": {"type": "string",
                                    "description": "Target channel (@name or id); defaults to the configured default channel."},
                        "text": {"type": "string", "minLength": 1}
                    },
                    "required": ["text"]
                }),
                annotations: json!({"readOnlyHint": false}),
                validate: validate_publish_post,
                handler: h(publish_post),
            },
            ToolSpec {
                name: "edit_post",
                description: "Edit the stored content of a logged post. Edits the stored \
                              record only; the already-sent Telegram message is not rewritten.",
                tier: Tier::Write,
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "post_id": {"type": "integer"},
                        "text": {"type": "string", "minLength": 1}
                    },
                    "required": ["post_id", "text"]
                }),
                annotations: json!({"readOnlyHint": false}),
                validate: validate_edit_post,
                handler: h(edit_post),
            },
            // ---- Tier: Sensitive (approval-gated; the call path never executes) ----
            ToolSpec {
                name: "gen_codes",
                description: "Generate activation codes. Requires owner approval; the full \
                              codes are delivered to the owner's Telegram chat and are never \
                              returned over MCP.",
                tier: Tier::Sensitive,
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "plan_id": {"type": "integer",
                                    "description": "Optional plan the codes unlock."},
                        "count": {"type": "integer", "minimum": 1, "maximum": MAX_CODES},
                        "days": {"type": "integer", "minimum": 1,
                                 "description": "Validity in days (when the code is not plan-bound)."},
                        "max_uses": {"type": "integer", "minimum": 1, "default": 1}
                    },
                    "required": ["count"]
                }),
                annotations: json!({"readOnlyHint": false, "destructiveHint": true,
                                    "x-approval-required": true}),
                validate: validate_gen_codes,
                handler: park_fn("gen_codes"),
            },
            ToolSpec {
                name: "delete_post",
                description: "Delete a stored post record. The stored record is deleted; the \
                              already-sent Telegram message is NOT unsent. Requires owner approval.",
                tier: Tier::Sensitive,
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "post_id": {"type": "integer"}
                    },
                    "required": ["post_id"]
                }),
                annotations: json!({"readOnlyHint": false, "destructiveHint": true,
                                    "x-approval-required": true}),
                validate: validate_delete_post,
                handler: park_fn("delete_post"),
            },
            ToolSpec {
                name: "broadcast",
                description: "Send a text message to all known users. High blast radius; \
                              requires owner approval.",
                tier: Tier::Sensitive,
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "text": {"type": "string", "minLength": 1}
                    },
                    "required": ["text"]
                }),
                annotations: json!({"readOnlyHint": false, "destructiveHint": true,
                                    "x-approval-required": true}),
                validate: validate_broadcast,
                handler: park_fn("broadcast"),
            },
        ]
    })
}

pub fn find_tool(name: &str) -> Option<&'static ToolSpec> {
    registry().iter().find(|t| t.name == name)
}

/// Box an async handler fn into the shared `Handler` shape.
fn h<F, Fut>(f: F) -> Handler
where
    F: Fn(WebState, Principal, Value) -> Fut + Copy + Send + Sync + 'static,
    Fut: Future<Output = Result<Value, String>> + Send + 'static,
{
    Arc::new(move |state, principal, args| Box::pin(f(state, principal, args)))
}

// ---- Argument validators (pure; run before any handler) ----

/// Tools that take no arguments accept only an absent or empty object.
fn validate_empty(args: &Value) -> Result<(), String> {
    match args {
        Value::Null | Value::Object(_) => Ok(()),
        _ => Err("arguments must be an object".into()),
    }
}

fn get_int<'a>(args: &'a Value, key: &str) -> Result<Option<&'a Value>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) if v.is_i64() => Ok(Some(v)),
        Some(_) => Err(format!("'{key}' must be an integer")),
    }
}

fn validate_limit(args: &Value) -> Result<(), String> {
    if let Some(v) = get_int(args, "limit")?
        && let Some(n) = v.as_i64()
        && !(1..=MAX_LIST_LIMIT).contains(&n)
    {
        return Err(format!("'limit' must be between 1 and {MAX_LIST_LIMIT}"));
    }
    Ok(())
}

fn validate_subs_report(args: &Value) -> Result<(), String> {
    match args.get("include_user_ids") {
        None | Some(Value::Null) => Ok(()),
        Some(v) if v.is_boolean() => Ok(()),
        Some(_) => Err("'include_user_ids' must be a boolean".into()),
    }
}

fn validate_text_field(args: &Value, key: &str) -> Result<(), String> {
    match args.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(()),
        Some(Value::String(_)) => Err(format!("'{key}' must not be empty")),
        Some(_) => Err(format!("'{key}' must be a string")),
        None => Err(format!("missing required argument '{key}'")),
    }
}

fn validate_publish_post(args: &Value) -> Result<(), String> {
    validate_text_field(args, "text")?;
    match args.get("channel") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(()),
        Some(_) => Err("'channel' must be a non-empty string".into()),
    }
}

fn validate_edit_post(args: &Value) -> Result<(), String> {
    if get_int(args, "post_id")?.is_none() {
        return Err("missing required integer argument 'post_id'".into());
    }
    validate_text_field(args, "text")
}

fn validate_gen_codes(args: &Value) -> Result<(), String> {
    let Some(v) = get_int(args, "count")? else {
        return Err("missing required integer argument 'count'".into());
    };
    let count = v.as_i64().unwrap_or(0);
    if !(1..=MAX_CODES).contains(&count) {
        return Err(format!("'count' must be between 1 and {MAX_CODES}"));
    }
    if let Some(v) = get_int(args, "days")?
        && v.as_i64().unwrap_or(0) < 1
    {
        return Err("'days' must be >= 1".into());
    }
    if let Some(v) = get_int(args, "max_uses")?
        && v.as_i64().unwrap_or(0) < 1
    {
        return Err("'max_uses' must be >= 1".into());
    }
    Ok(())
}

fn validate_delete_post(args: &Value) -> Result<(), String> {
    if get_int(args, "post_id")?.is_none() {
        return Err("missing required integer argument 'post_id'".into());
    }
    Ok(())
}

fn validate_broadcast(args: &Value) -> Result<(), String> {
    validate_text_field(args, "text")
}

// ---- Helpers shared by handlers ----

/// Mask an activation code for every read path: keep the first 4 characters,
/// never the rest. Codes are 12 ASCII chars from `util::gen_token`, so
/// char-based truncation is exact.
fn mask_code(code: &str) -> String {
    let head: String = code.chars().take(4).collect();
    format!("{head}…")
}

fn list_limit(args: &Value) -> i64 {
    args.get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .clamp(1, MAX_LIST_LIMIT)
}

/// Park a sensitive call: nothing executes here, the args are stored verbatim
/// so the owner's approval replays exactly what the caller asked for.
async fn park(
    state: WebState,
    _principal: Principal,
    args: Value,
    tool: &'static str,
) -> Result<Value, String> {
    let args_json = serde_json::to_string(&args).unwrap_or_else(|_| "{}".into());
    let expires_at =
        (chrono::Utc::now() + chrono::Duration::seconds(APPROVAL_TTL_SECS)).to_rfc3339();
    let id = state.db.mcp_create_approval(tool, &args_json, &expires_at);
    Ok(json!({
        "requires_approval": true,
        "approval_id": id,
        "expires_at": expires_at,
        "how": "approve via /approve <id> in the bot (owner only)"
    }))
}

/// Build the park handler for a sensitive tool name.
fn park_fn(tool: &'static str) -> Handler {
    h(move |state, principal, args| park(state, principal, args, tool))
}

// ---- Tier Read handlers ----

async fn list_channels(
    state: WebState,
    principal: Principal,
    _args: Value,
) -> Result<Value, String> {
    let default = state.db.get_config("channel");
    let sellers: Vec<Value> = if let Some(sid) = principal.seller_id {
        vec![json!({"seller_id": sid, "channel": state.db.seller_channel(sid)})]
    } else {
        state
            .db
            .list_sellers()
            .into_iter()
            .map(|sid| json!({"seller_id": sid, "channel": state.db.seller_channel(sid)}))
            .collect()
    };
    Ok(json!({"default": default, "sellers": sellers}))
}

async fn list_plans(state: WebState, principal: Principal, _args: Value) -> Result<Value, String> {
    // PlanRow: (name, price, asset, duration_days, channel_id, active, seller_id)
    let items: Vec<Value> = state
        .db
        .list_plans(principal.seller_id)
        .into_iter()
        .map(|p| {
            json!({
                "name": p.0, "price": p.1, "asset": p.2, "duration_days": p.3,
                "channel_id": p.4, "active": p.5, "seller_id": p.6,
            })
        })
        .collect();
    Ok(json!({"items": items, "total": items.len()}))
}

async fn list_posts(state: WebState, principal: Principal, args: Value) -> Result<Value, String> {
    // PostRow: (id, seller_id, chat_id, message_id, kind, content, created_at)
    let items: Vec<Value> = state
        .db
        .list_posts(principal.seller_id, list_limit(&args))
        .into_iter()
        .map(|p| {
            json!({
                "id": p.0, "seller_id": p.1, "chat_id": p.2, "message_id": p.3,
                "kind": p.4, "content": p.5, "created_at": p.6,
            })
        })
        .collect();
    Ok(json!({"items": items, "total": items.len()}))
}

async fn subs_report(state: WebState, principal: Principal, args: Value) -> Result<Value, String> {
    let include_ids = args
        .get("include_user_ids")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let scope = principal.seller_id;
    let by_status: Value = state
        .db
        .subscription_status_counts(scope)
        .into_iter()
        .map(|(status, count)| (status, Value::from(count)))
        .collect::<serde_json::Map<String, Value>>()
        .into();
    if !include_ids {
        // Default: aggregates only — no subscriber identities leave the box.
        return Ok(json!({"by_status": by_status, "contains_pii": false}));
    }
    // list_subscriptions row: (id, seller_id, user_id, channel_id, status, expires_at)
    let rows: Vec<Value> = state
        .db
        .list_subscriptions(scope, 100)
        .into_iter()
        .map(|s| {
            json!({
                "id": s.0, "seller_id": s.1, "user_id": s.2, "channel_id": s.3,
                "status": s.4, "expires_at": s.5,
            })
        })
        .collect();
    Ok(json!({
        "by_status": by_status,
        "rows": rows,
        "contains_pii": true
    }))
}

async fn list_codes(state: WebState, principal: Principal, args: Value) -> Result<Value, String> {
    // CodeListRow: (code, plan_id, value_days, uses, max_uses, seller_id)
    let items: Vec<Value> = state
        .db
        .list_codes(principal.seller_id, list_limit(&args))
        .into_iter()
        .map(|c| {
            json!({
                "code": mask_code(&c.0), "plan_id": c.1, "value_days": c.2,
                "uses": c.3, "max_uses": c.4, "seller_id": c.5,
            })
        })
        .collect();
    Ok(json!({"items": items, "total": items.len()}))
}

// ---- Tier Write handlers ----

async fn publish_post(state: WebState, principal: Principal, args: Value) -> Result<Value, String> {
    // Mirrors the web API's posts_create: arg channel > configured default.
    let ch = args
        .get("channel")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| state.db.get_config("channel").unwrap_or_default());
    if ch.is_empty() {
        return Err("no channel configured (set the default channel first)".into());
    }
    let text = args
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    // Unlike the web API (which logs a 0 message id), a bot failure is
    // reported as a tool error so an AI caller knows nothing was posted.
    match state
        .bot
        .send_message(crate::util::to_chat_id(&ch), text.clone())
        .await
    {
        Ok(sent) => {
            let seller_id = principal.seller_id.unwrap_or(0);
            let _ = state
                .db
                .log_post(seller_id, &ch, sent.id.0, "text", Some(&text));
            Ok(json!({"ok": true, "message_id": sent.id.0, "channel": ch}))
        }
        Err(e) => Err(format!("send failed: {e}")),
    }
}

async fn edit_post(state: WebState, _principal: Principal, args: Value) -> Result<Value, String> {
    let post_id = args
        .get("post_id")
        .and_then(|v| v.as_i64())
        .unwrap_or_default();
    let text = args
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    match state.db.update_post_content(post_id, text) {
        Ok(()) => Ok(json!({"ok": true, "post_id": post_id,
                            "note": "stored record updated; the sent Telegram message was not rewritten"})),
        Err(e) => Err(format!("update failed: {e}")),
    }
}

// ---- Tier Sensitive executors (approval path only — see execute_sensitive) ----

async fn exec_gen_codes(
    state: WebState,
    principal: Principal,
    args: Value,
) -> Result<Value, String> {
    let count = args.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
    let plan_id = args.get("plan_id").and_then(|v| v.as_i64());
    let days = args
        .get("days")
        .and_then(|v| v.as_i64())
        .map(|d| d.clamp(1, crate::handlers::commands::MAX_PLAN_DAYS));
    let max_uses = args.get("max_uses").and_then(|v| v.as_i64()).unwrap_or(1);
    let seller_id = principal.seller_id.unwrap_or(0);
    let codes: Vec<String> = (0..count)
        .map(|_| {
            state
                .db
                .create_code(seller_id, plan_id, days, max_uses, None)
        })
        .collect();
    Ok(json!({
        "codes": codes,
        "count": codes.len(),
        "note": "codes are secrets: deliver them only to the intended buyers"
    }))
}

async fn exec_delete_post(
    state: WebState,
    _principal: Principal,
    args: Value,
) -> Result<Value, String> {
    let post_id = args
        .get("post_id")
        .and_then(|v| v.as_i64())
        .unwrap_or_default();
    state
        .db
        .delete_post(post_id)
        .map_err(|e| format!("delete failed: {e}"))?;
    Ok(json!({"ok": true, "post_id": post_id,
              "note": "stored record deleted; the sent Telegram message was not unsent"}))
}

async fn exec_broadcast(
    state: WebState,
    principal: Principal,
    args: Value,
) -> Result<Value, String> {
    let text = args
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let users = match principal.seller_id {
        Some(sid) => state.db.list_users_for_seller(sid),
        None => state.db.list_users(),
    };
    let total = users.len();
    // Send loop shared with the /broadcast command (crate::broadcast_to_users):
    // undeliverable users are skipped, the rest still get the message.
    let sent = crate::broadcast_to_users(&state.bot, &users, text).await;
    Ok(json!({"sent": sent, "total": total}))
}

/// Execute an approved sensitive tool. This is the ONLY path that runs the
/// sensitive executors, and it re-runs the tool's own validator first so the
/// approval replay can never drift from what the call path would have done.
pub async fn execute_sensitive(
    state: WebState,
    principal: &Principal,
    tool: &str,
    args: Value,
) -> Result<Value, String> {
    let spec = find_tool(tool).ok_or_else(|| format!("unknown tool '{tool}'"))?;
    if spec.tier != Tier::Sensitive {
        return Err(format!("tool '{tool}' is not approval-gated"));
    }
    (spec.validate)(&args)?;
    match tool {
        "gen_codes" => exec_gen_codes(state, principal.clone(), args).await,
        "delete_post" => exec_delete_post(state, principal.clone(), args).await,
        "broadcast" => exec_broadcast(state, principal.clone(), args).await,
        other => Err(format!("no executor for tool '{other}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_code_keeps_only_the_head() {
        assert_eq!(mask_code("ABCD1234EFGH"), "ABCD…");
        // Short/odd inputs must not panic and must not reveal more than asked.
        assert_eq!(mask_code("AB"), "AB…");
    }

    #[test]
    fn registry_tiers_and_annotations() {
        for t in registry() {
            match t.tier {
                Tier::Read => assert_eq!(
                    t.annotations["readOnlyHint"], true,
                    "{} must be readOnly",
                    t.name
                ),
                Tier::Write => assert_eq!(t.annotations["readOnlyHint"], false),
                Tier::Sensitive => {
                    assert_eq!(t.annotations["destructiveHint"], true, "{}", t.name);
                    assert_eq!(t.annotations["x-approval-required"], true, "{}", t.name);
                }
            }
        }
    }

    /// The gen_codes schema must not carry any string default: a secret-bearing
    /// default would embed a real code shape in the advertised tool metadata.
    #[test]
    fn gen_codes_schema_has_no_string_defaults() {
        let spec = find_tool("gen_codes").unwrap();
        for (k, v) in spec.input_schema["properties"].as_object().unwrap() {
            assert!(
                v.get("default").map(|d| !d.is_string()).unwrap_or(true),
                "property {k} has a string default"
            );
        }
    }

    #[test]
    fn validators_reject_bad_shapes() {
        assert!(validate_gen_codes(&json!({"count": 0})).is_err());
        assert!(validate_gen_codes(&json!({"count": 51})).is_err());
        assert!(validate_gen_codes(&json!({"count": "3"})).is_err());
        assert!(validate_gen_codes(&json!({"count": 5, "days": 0})).is_err());
        assert!(validate_gen_codes(&json!({"count": 5})).is_ok());
        assert!(validate_publish_post(&json!({"text": "  "})).is_err());
        assert!(validate_publish_post(&json!({"text": "hi"})).is_ok());
        assert!(validate_broadcast(&json!({})).is_err());
        assert!(validate_delete_post(&json!({})).is_err());
        assert!(validate_delete_post(&json!({"post_id": 3})).is_ok());
        assert!(validate_limit(&json!({"limit": 51})).is_err());
        assert!(validate_limit(&json!({})).is_ok());
    }
}
