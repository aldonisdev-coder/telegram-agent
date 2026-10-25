//! Embed the agent's dashboard + API under `/agent` inside a host app.
//!
//! Run with: `cargo run --example embedded_host`

use axum::{Router, routing::get};
use telegram_agent::embed::AgentAppBuilder;

#[tokio::main]
async fn main() {
    let app = AgentAppBuilder::new()
        .base_path("/agent")
        .db_path("data/host.db")
        .build()
        .await
        .expect("build agent app");

    // `app.router()` is already mounted under the app's base path, so the
    // host merges it (nesting again would double the prefix).
    let router = Router::new()
        .route("/", get(|| async { "host app" }))
        .merge(app.router());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000")
        .await
        .expect("bind 0.0.0.0:3000");
    println!("listening on http://127.0.0.1:3000 (agent under /agent)");
    axum::serve(listener, router).await.expect("server error");
}
