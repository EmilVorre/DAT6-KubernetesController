//! Optional health/liveness endpoints (enable with `health` feature).
//!
//! Serves e.g. `/live` and `/ready` for Kubernetes probes.

use axum::{routing::get, Router};
use std::net::SocketAddr;
use tracing::info;

/// Builds the health check router (live + ready).
pub fn router() -> Router {
    Router::new()
        .route("/live", get(|| async { "ok" }))
        .route("/ready", get(|| async { "ok" }))
}

/// Spawns the health server in the background. Returns the join handle.
pub fn spawn_server(addr: SocketAddr) -> tokio::task::JoinHandle<()> {
    let app = router();
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        info!(%addr, "health server listening");
        axum::serve(listener, app).await.unwrap();
    })
}
