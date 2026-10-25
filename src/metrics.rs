//! Hand-rolled Prometheus metrics (PLAN.md observability queue: "Metrics/
//! tracing spans for the performance budgets"). Zero new dependencies: a
//! `OnceLock<Metrics>` of atomics, scraped as Prometheus text exposition at
//! `GET /metrics` (registered in `api::router`, no auth — the default bind is
//! 127.0.0.1; this endpoint MUST NOT be exposed publicly).
//!
//! SECURITY INVARIANT — house lesson from a sister project's real incident:
//! a metrics endpoint leaked subscription tokens because a label fell back
//! to the RAW request path for unmatched routes. Labels here come ONLY from
//! fixed categories derived from axum's `MatchedPath` (the registered ROUTE
//! TEMPLATE, e.g. `/api/v1/codes/{code}`) or from fixed literals. NEVER a
//! raw request path, NEVER a query string, NEVER a header value, and never a
//! caller-supplied string (unknown MCP tool names land in the fixed
//! `"unknown"` bucket). `no_raw_path_label_for_unmatched_routes` in
//! `api.rs`'s tests pins this with a token-bearing request.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;

// ---- Fixed route categories -----------------------------------------------
//
// The table is closed: `classify_name` can only ever return one of these
// literals (or "other"), so no dynamic string can ever become a label value.

const ROUTE_CATEGORIES: &[&str] = &[
    // mounted control-plane endpoints
    "health",
    "metrics",
    "mcp",
    // /api/v1/{group}/... — group comes from the registered route template
    "api_v1_auth",
    "api_v1_sellers",
    "api_v1_plans",
    "api_v1_subscriptions",
    "api_v1_users",
    "api_v1_invoices",
    "api_v1_webhooks",
    "api_v1_codes",
    "api_v1_posts",
    "api_v1_broadcasts",
    "api_v1_dashboard",
    "api_v1_revenue",
    "api_v1_channels",
    "api_v1_config",
    "api_v1_ai",
    "api_v1_products",
    "api_v1_shop",
    "api_v1_finance",
    "api_v1_admin",
    // web dashboard pages (web.rs)
    "web_index",
    "web_setup",
    "web_login",
    "web_logout",
    "web_dashboard",
    "web_channels",
    "web_plans",
    "web_posts",
    "web_post",
    "web_plan",
    "web_plandel",
    "web_products",
    "web_product",
    "web_productdel",
    "web_static",
    // catch-all for unmatched routes and anything not listed above
    "other",
];

const N_ROUTES: usize = ROUTE_CATEGORIES.len();

const STATUS_CLASSES: usize = 5;
const STATUS_CLASS_NAMES: [&str; STATUS_CLASSES] = ["1xx", "2xx", "3xx", "4xx", "5xx"];

/// Tier slots for `mcp_tool_calls_total`: read, write, sensitive, unknown.
const MCP_TIERS: usize = 4;
const MCP_TIER_NAMES: [&str; MCP_TIERS] = ["read", "write", "sensitive", "unknown"];

/// Map a response status to its 1xx..5xx class index.
fn status_class_index(status: axum::http::StatusCode) -> usize {
    match status.as_u16() / 100 {
        1 => 0,
        2 => 1,
        3 => 2,
        4 => 3,
        _ => 4,
    }
}

/// Classify a matched route template into a fixed category index.
///
/// The input is axum's `MatchedPath` — the REGISTERED route pattern (like
/// `/api/v1/codes/{code}`), never the raw URI. Every return value is a fixed
/// literal from `ROUTE_CATEGORIES`: no raw path, query string or header value
/// can ever become a label, even for unmatched routes (which have no
/// `MatchedPath` at all and land in "other").
pub(crate) fn classify_route(matched: Option<&str>) -> usize {
    let name = classify_name(matched);
    // classify_name is total over ROUTE_CATEGORIES, so position() always
    // hits; the fallback keeps the invariant even if the two drift.
    ROUTE_CATEGORIES
        .iter()
        .position(|c| *c == name)
        .unwrap_or(N_ROUTES - 1)
}

fn classify_name(matched: Option<&str>) -> &'static str {
    let Some(t) = matched else { return "other" };
    // API groups: /api/v1/{group}/... — the group segment comes from the
    // registered template, so it is one of the router's fixed names.
    if let Some(rest) = t.strip_prefix("/api/v1/") {
        let group = rest.split('/').next().unwrap_or("");
        return match group {
            "mcp" => "mcp",
            "auth" => "api_v1_auth",
            "sellers" => "api_v1_sellers",
            "plans" => "api_v1_plans",
            "subscriptions" => "api_v1_subscriptions",
            "users" => "api_v1_users",
            "invoices" => "api_v1_invoices",
            "webhooks" => "api_v1_webhooks",
            "codes" => "api_v1_codes",
            "posts" => "api_v1_posts",
            "broadcasts" => "api_v1_broadcasts",
            "dashboard" => "api_v1_dashboard",
            "revenue" => "api_v1_revenue",
            "channels" => "api_v1_channels",
            "config" => "api_v1_config",
            "ai" => "api_v1_ai",
            "products" => "api_v1_products",
            "shop" => "api_v1_shop",
            "finance" => "api_v1_finance",
            "admin" => "api_v1_admin",
            _ => "other",
        };
    }
    // Non-API: exact health/metrics, then the web dashboard's first segment
    // (also from the registered template).
    let seg = t.trim_start_matches('/').split('/').next().unwrap_or("");
    match seg {
        "" => "web_index",
        "health" => "health",
        "metrics" => "metrics",
        "setup" => "web_setup",
        "login" => "web_login",
        "logout" => "web_logout",
        "dashboard" => "web_dashboard",
        "channels" => "web_channels",
        "plans" => "web_plans",
        "posts" => "web_posts",
        "post" => "web_post",
        "plan" => "web_plan",
        "plandel" => "web_plandel",
        "products" => "web_products",
        "product" => "web_product",
        "productdel" => "web_productdel",
        "static" => "web_static",
        _ => "other",
    }
}

// ---- Registry --------------------------------------------------------------

/// One `mcp_tool_calls_total` row: per-tier, per-outcome atomics.
struct McpRow {
    /// `None` is the fixed "unknown tool" row; caller-supplied tool names are
    /// NEVER used as label values (same invariant as routes above).
    name: Option<&'static str>,
    calls: [[AtomicU64; 2]; MCP_TIERS],
}

pub struct Metrics {
    start: Instant,
    /// `http_requests_total{route,status_class}` — [route][status_class].
    http: [[AtomicU64; STATUS_CLASSES]; N_ROUTES],
    /// `mcp_tool_calls_total{tool,tier,ok}` — registry tools + "unknown".
    mcp: Vec<McpRow>,
    /// `bot_messages_sent_total`.
    bot: AtomicU64,
}

fn zeroed<const N: usize>() -> [AtomicU64; N] {
    std::array::from_fn(|_| AtomicU64::new(0))
}

fn global() -> &'static Metrics {
    static METRICS: OnceLock<Metrics> = OnceLock::new();
    METRICS.get_or_init(|| Metrics {
        start: Instant::now(),
        http: std::array::from_fn(|_| zeroed()),
        // The MCP tool table is static; deriving the rows from it keeps the
        // labels in lock-step with the registry, plus one "unknown" row.
        mcp: {
            let mut rows: Vec<McpRow> = crate::mcp::tools::registry()
                .iter()
                .map(|t| McpRow {
                    name: Some(t.name),
                    calls: std::array::from_fn(|_| zeroed()),
                })
                .collect();
            rows.push(McpRow {
                name: None,
                calls: std::array::from_fn(|_| zeroed()),
            });
            rows
        },
        bot: AtomicU64::new(0),
    })
}

/// Pin the process-start instant early (called once from `main`) so the
/// uptime gauge is accurate even before the first HTTP request.
pub fn note_process_start() {
    let _ = global();
}

/// Seconds since process start (the `uptime_seconds` gauge value).
pub fn uptime_secs() -> u64 {
    global().start.elapsed().as_secs()
}

/// Increment `http_requests_total{route,status_class}` — normally called by
/// the `track_http` middleware, not by hand.
pub(crate) fn http(route: usize, status: axum::http::StatusCode) {
    global().http[route][status_class_index(status)].fetch_add(1, Ordering::Relaxed);
}

/// Increment `mcp_tool_calls_total{tool,tier,ok}`.
///
/// `tool = None` (or a name outside the registry) records the fixed
/// "unknown" row; `tier` beyond read/write/sensitive records "unknown".
/// Caller-supplied strings are deliberately NOT accepted as label values.
pub fn mcp_tool(tool: Option<&'static str>, tier: &'static str, ok: bool) {
    let m = global();
    let ti = match tier {
        "read" => 0,
        "write" => 1,
        "sensitive" => 2,
        _ => 3,
    };
    let oi = usize::from(!ok); // column 0 = ok, column 1 = failed
    // Unregistered names fall back to the trailing "unknown" row (always
    // present; if it were somehow missing we simply drop the count rather
    // than panic on a hot path).
    if let Some(row) = m
        .mcp
        .iter()
        .find(|r| r.name == tool)
        .or_else(|| m.mcp.last())
    {
        row.calls[ti][oi].fetch_add(1, Ordering::Relaxed);
    }
}

/// Increment `bot_messages_sent_total` after a successful bot send.
///
/// Wired only at the send sites in `main.rs` (background granter + scheduled
/// posts); the per-handler send sites in `handlers/` are intentionally not
/// instrumented yet (other rung's territory).
pub fn bot_sent() {
    global().bot.fetch_add(1, Ordering::Relaxed);
}

/// The HTTP metrics middleware: classifies by MATCHED ROUTE TEMPLATE only
/// (see the SECURITY INVARIANT at the top of this module), runs the inner
/// stack, then bumps the counter for the response's status class.
///
/// Applied outermost on the merged router in `main.rs` so requests aborted by
/// the timeout layer are still counted as 5xx.
pub async fn track_http(req: Request, next: Next) -> Response {
    // `MatchedPath` is placed in request extensions by the router before
    // `Router::layer` middleware runs; fallback (unmatched) requests have no
    // entry and classify as "other".
    let route = classify_route(req.extensions().get::<MatchedPath>().map(|m| m.as_str()));
    let resp = next.run(req).await;
    http(route, resp.status());
    resp
}

/// `GET /metrics` handler: the registry rendered as Prometheus text
/// exposition. Deliberately unauthenticated — the default bind is
/// 127.0.0.1 and this endpoint must never be exposed publicly.
pub async fn render_metrics() -> impl axum::response::IntoResponse {
    (
        axum::http::StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        render(),
    )
}

/// Prometheus text exposition (format 0.0.4) of the registry.
pub fn render() -> String {
    let m = global();
    let mut out = String::with_capacity(2048);

    out.push_str("# HELP http_requests_total HTTP requests handled, by fixed route category and status class.\n");
    out.push_str("# TYPE http_requests_total counter\n");
    for (ri, name) in ROUTE_CATEGORIES.iter().enumerate() {
        for (ci, cls) in STATUS_CLASS_NAMES.iter().enumerate() {
            let v = m.http[ri][ci].load(Ordering::Relaxed);
            if v > 0 {
                out.push_str(&format!(
                    "http_requests_total{{route=\"{name}\",status_class=\"{cls}\"}} {v}\n"
                ));
            }
        }
    }

    out.push_str(
        "# HELP mcp_tool_calls_total MCP tools/call outcomes, by tool, tier and outcome.\n",
    );
    out.push_str("# TYPE mcp_tool_calls_total counter\n");
    for row in &m.mcp {
        let tool = row.name.unwrap_or("unknown");
        for (ti, tier) in MCP_TIER_NAMES.iter().enumerate() {
            for (oi, ok) in ["true", "false"].iter().enumerate() {
                let v = row.calls[ti][oi].load(Ordering::Relaxed);
                if v > 0 {
                    out.push_str(&format!(
                        "mcp_tool_calls_total{{tool=\"{tool}\",tier=\"{tier}\",ok=\"{ok}\"}} {v}\n"
                    ));
                }
            }
        }
    }

    out.push_str("# HELP bot_messages_sent_total Bot messages sent by tracked send sites.\n");
    out.push_str("# TYPE bot_messages_sent_total counter\n");
    out.push_str(&format!(
        "bot_messages_sent_total {}\n",
        m.bot.load(Ordering::Relaxed)
    ));

    out.push_str("# HELP uptime_seconds Seconds since the server process started.\n");
    out.push_str("# TYPE uptime_seconds gauge\n");
    out.push_str(&format!("uptime_seconds {}\n", uptime_secs()));

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::get;
    use tower::ServiceExt;

    fn app() -> Router {
        Router::new()
            .route("/ping", get(|| async { "pong" }))
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    "done"
                }),
            )
            .layer(axum::middleware::from_fn(track_http))
            .route("/metrics", get(super::render_metrics))
    }

    #[tokio::test]
    async fn scrape_contains_expected_series() {
        let app = app();
        for _ in 0..2 {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/ping")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), axum::http::StatusCode::OK);
        }
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body).to_string();
        // Unmatched route category ("/ping" is not a registered dashboard page).
        // (Presence, not an exact count: the registry is process-global and
        // other tests in this binary may bump it concurrently.)
        assert!(text.contains("http_requests_total{route=\"other\",status_class=\"2xx\"}"));
        // HELP/TYPE blocks and the scalar series.
        assert!(text.contains("# TYPE http_requests_total counter"));
        assert!(text.contains("# HELP uptime_seconds"));
        assert!(text.contains("# TYPE bot_messages_sent_total counter"));
        assert!(
            text.starts_with("bot_messages_sent_total")
                || text.contains("\nbot_messages_sent_total ")
        );
        let up: u64 = text
            .lines()
            .find_map(|l| l.strip_prefix("uptime_seconds "))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        assert!(up < 3600, "uptime gauge should be small in tests, got {up}");
    }

    #[tokio::test]
    async fn no_raw_path_label_for_unmatched_routes() {
        // House-lesson regression test: a token-bearing path (and query
        // string) hitting an UNMATCHED route must never appear as a label.
        let secret = "sub_tok_9f8e7d6c5b4a";
        let app = app();
        let uri = format!("/unmatched/leak/{secret}?token={secret}");
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri.as_str())
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body).to_string();
        assert!(
            !text.contains(secret),
            "metrics output leaked the raw path/query: {text}"
        );
        assert!(text.contains("http_requests_total{route=\"other\",status_class=\"4xx\"}"));
    }

    #[test]
    fn mcp_counter_labels_stay_fixed() {
        // Unknown tool names must land in the fixed "unknown" bucket.
        mcp_tool(None, "unknown", false);
        mcp_tool(Some("gen_codes"), "sensitive", true);
        mcp_tool(Some("gen_codes"), "sensitive", false);
        let text = render();
        assert!(
            text.contains("mcp_tool_calls_total{tool=\"unknown\",tier=\"unknown\",ok=\"false\"}")
        );
        assert!(
            text.contains(
                "mcp_tool_calls_total{tool=\"gen_codes\",tier=\"sensitive\",ok=\"true\"}"
            )
        );
        assert!(
            text.contains(
                "mcp_tool_calls_total{tool=\"gen_codes\",tier=\"sensitive\",ok=\"false\"}"
            )
        );
    }

    #[test]
    fn classify_maps_templates_not_paths() {
        assert_eq!(
            ROUTE_CATEGORIES[classify_route(Some("/api/v1/codes/{code}"))],
            "api_v1_codes"
        );
        assert_eq!(ROUTE_CATEGORIES[classify_route(Some("/api/v1/mcp"))], "mcp");
        assert_eq!(ROUTE_CATEGORIES[classify_route(Some("/health"))], "health");
        assert_eq!(ROUTE_CATEGORIES[classify_route(Some("/"))], "web_index");
        assert_eq!(ROUTE_CATEGORIES[classify_route(None)], "other");
        // Even a hostile "template" that is really a raw path collapses to a
        // fixed category (the known api group), never the path itself.
        let hostile = "/api/v1/codes/sub_tok_9f8e7d6c5b4a";
        assert_eq!(
            ROUTE_CATEGORIES[classify_route(Some(hostile))],
            "api_v1_codes"
        );
        // ...while an unknown group falls into the catch-all.
        assert_eq!(
            ROUTE_CATEGORIES[classify_route(Some("/api/v1/sub_tok_9f8e7d6c5b4a"))],
            "other"
        );
    }
}
