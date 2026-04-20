//! Drainable HTTP service — baseline for safe decomposition experiments.
//!
//! - GET / — returns 200 with configurable latency (SLEEP_MS env)
//! - GET /metrics — Prometheus metrics (in-flight, total requests, errors)
//! - GET /healthz — always OK while running
//! - GET /readyz — always OK (controller owns readiness removal)
//! - GET /drainez — reports active connections and deletion readiness

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use prometheus::{Encoder, IntCounterVec, IntGauge, Opts, TextEncoder};
use tower::ServiceBuilder;
use tower_http::timeout::TimeoutLayer;
use tracing::info;

/// Shared app state
#[derive(Clone)]
struct AppState {
    /// In-flight request count
    in_flight: IntGauge,
    /// Total requests
    total_requests: IntCounterVec,
    /// Errors
    errors: IntCounterVec,
    /// Sleep ms for GET /
    sleep_ms: u64,
}

static IN_FLIGHT_COUNT: AtomicU64 = AtomicU64::new(0);

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let sleep_ms: u64 = std::env::var("SLEEP_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    // Long-requests mode: 5% of requests sleep 2–10s (fault injection)
    let long_requests_pct: u32 = std::env::var("LONG_REQUESTS_PCT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    // Burst mode: simulate RPS spikes via env (used by load generator; service just logs)
    let _burst_mode = std::env::var("BURST_MODE").is_ok();

    // Prometheus metrics (register to default registry)
    let in_flight = IntGauge::with_opts(Opts::new(
        "drainable_in_flight_requests",
        "Number of requests currently being processed",
    ))
    .unwrap();
    prometheus::default_registry()
        .register(Box::new(in_flight.clone()))
        .unwrap();

    let total_requests = IntCounterVec::new(
        Opts::new("drainable_requests_total", "Total requests by path and status"),
        &["path", "status"],
    )
    .unwrap();
    prometheus::default_registry()
        .register(Box::new(total_requests.clone()))
        .unwrap();

    let errors = IntCounterVec::new(
        Opts::new("drainable_errors_total", "Total errors by path"),
        &["path"],
    )
    .unwrap();
    prometheus::default_registry()
        .register(Box::new(errors.clone()))
        .unwrap();

    let state = AppState {
        in_flight,
        total_requests,
        errors,
        sleep_ms,
    };

    let app = Router::new()
        .route("/", get(root_handler))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/drainez", get(drainez))
        .route("/metrics", get(metrics_handler))
        .layer(ServiceBuilder::new().layer(TimeoutLayer::new(Duration::from_secs(30))))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
    info!("drainable-service listening on 0.0.0.0:8080 (SLEEP_MS={}, LONG_REQUESTS_PCT={})", sleep_ms, long_requests_pct);

    axum::serve(listener, app).await.unwrap();
}

async fn root_handler(State(state): State<AppState>) -> impl IntoResponse {
    struct InFlightGuard<'a> {
        state: &'a AppState,
    }
    impl Drop for InFlightGuard<'_> {
        fn drop(&mut self) {
            IN_FLIGHT_COUNT.fetch_sub(1, Ordering::SeqCst);
            self.state.in_flight.dec();
        }
    }
    IN_FLIGHT_COUNT.fetch_add(1, Ordering::SeqCst);
    state.in_flight.inc();
    let _guard = InFlightGuard { state: &state };

    let sleep_duration = if state.sleep_ms > 0 {
        Duration::from_millis(state.sleep_ms)
    } else {
        // Fault injection: 5% long requests (2–10s) when LONG_REQUESTS_PCT=5
        let pct: u32 = std::env::var("LONG_REQUESTS_PCT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if pct > 0 && (rand_simple() % 100) < pct {
            let secs = 2 + (rand_simple() % 9) as u64;
            Duration::from_secs(secs)
        } else {
            Duration::ZERO
        }
    };

    if !sleep_duration.is_zero() {
        tokio::time::sleep(sleep_duration).await;
    }

    state
        .total_requests
        .with_label_values(&["/", "200"])
        .inc();

    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn readyz(State(_state): State<AppState>) -> impl IntoResponse {
    (StatusCode::OK, "ok").into_response()
}

async fn drainez() -> impl IntoResponse {
    let active_connections = IN_FLIGHT_COUNT.load(Ordering::SeqCst);
    let ready_to_delete = active_connections == 0;
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "active_connections": active_connections,
            "draining": true,
            "ready_to_delete": ready_to_delete
        })),
    )
}

async fn metrics_handler() -> impl IntoResponse {
    let encoder = TextEncoder::new();
    let families = prometheus::gather();
    let mut buffer = Vec::new();
    encoder.encode(&families, &mut buffer).unwrap();
    (
        [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        buffer,
    )
        .into_response()
}

/// Simple deterministic-ish random for fault injection (avoid extra dep)
fn rand_simple() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        % 100) as u32
}
