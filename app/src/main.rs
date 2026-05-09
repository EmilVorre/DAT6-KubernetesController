//! Drainable HTTP service — baseline for safe decomposition experiments.
//!
//! Endpoints:
//! - `GET /`         — returns 200 with configurable latency (`SLEEP_MS` env)
//! - `GET /metrics`  — Prometheus metrics (in-flight, total requests, errors)
//! - `GET /healthz`  — always OK while the process is alive
//! - `GET /readyz`   — **always 200 by design**. The custom readiness gate
//!   `decomposition.dat6.io/drain` (set by the controller, not the app) is what
//!   removes the pod from Service endpoints in S1/S2. We deliberately do *not*
//!   flip `/readyz` to 503 on shutdown so the comparison between baseline /
//!   S1 / S2 isolates the *controller's* contribution; if the app itself
//!   demoted `/readyz`, kube-proxy would remove the endpoint regardless of
//!   strategy and there would be nothing to measure. Trade-off: an unhealthy
//!   app cannot self-evict from endpoints.
//! - `GET /drainez`  — reports active in-flight count, whether the process has
//!   received SIGTERM (`draining`), and whether it is safe for the controller
//!   to remove its finalizer (`ready_to_delete`).
//!
//! Graceful shutdown is **opt-in** via `DAT6_GRACEFUL_DRAIN=1`. When unset
//! (the baseline configuration), the process inherits the kernel default for
//! SIGTERM and dies immediately, dropping in-flight requests — which is what
//! we want to expose as the "without the controller" failure mode. S1/S2
//! overlays set `DAT6_GRACEFUL_DRAIN=1` so the app stays alive long enough
//! for in-flight work to complete (S1) and for the controller to verify
//! `/drainez` (S2).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

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

#[derive(Clone)]
struct AppState {
    in_flight: IntGauge,
    total_requests: IntCounterVec,
    errors: IntCounterVec,
    sleep_ms: u64,
}

static IN_FLIGHT_COUNT: AtomicU64 = AtomicU64::new(0);
/// Set to true once the process has received SIGTERM. Surfaced via `/drainez`
/// so the controller and external observers can see whether termination has
/// actually started.
static IS_DRAINING: AtomicBool = AtomicBool::new(false);

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

    // Opt-in graceful drain. When enabled the app installs a SIGTERM handler,
    // flips `IS_DRAINING`, and asks axum to keep serving in-flight requests
    // until they all complete (or `DAT6_DRAIN_MAX_SECS` elapses) before
    // returning. Without this the process exits on SIGTERM via the default
    // kernel handler — which is the "baseline" failure mode we want to
    // measure against.
    let graceful_drain: bool = std::env::var("DAT6_GRACEFUL_DRAIN")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    // Upper bound on how long we keep serving after SIGTERM. Should be a few
    // seconds less than the pod spec's `terminationGracePeriodSeconds` so the
    // app can exit cleanly before the kubelet sends SIGKILL.
    let drain_max_secs: u64 = std::env::var("DAT6_DRAIN_MAX_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(28);

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
    info!(
        sleep_ms,
        long_requests_pct,
        graceful_drain,
        drain_max_secs,
        "drainable-service listening on 0.0.0.0:8080"
    );

    if graceful_drain {
        let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal(
            Duration::from_secs(drain_max_secs),
        ));
        if let Err(e) = server.await {
            tracing::error!(error = %e, "server exited with error");
        }
    } else {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "server exited with error");
        }
    }
}

/// Returns once the process should stop accepting new connections. Listens for
/// SIGTERM (and Ctrl-C in dev), flips `IS_DRAINING`, then waits for in-flight
/// requests to drain before resolving (or until `drain_max` elapses).
async fn shutdown_signal(drain_max: Duration) {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");

    tokio::select! {
        _ = sigterm.recv() => info!("received SIGTERM"),
        _ = sigint.recv()  => info!("received SIGINT"),
    }
    IS_DRAINING.store(true, Ordering::SeqCst);
    info!("draining: keeping the server alive until in-flight requests complete");

    let start = Instant::now();
    loop {
        let in_flight = IN_FLIGHT_COUNT.load(Ordering::SeqCst);
        if in_flight == 0 {
            info!("drain complete; in-flight = 0");
            tokio::time::sleep(Duration::from_secs(2)).await;
            break;
        }
        if start.elapsed() >= drain_max {
            tracing::warn!(in_flight, "drain timed out; proceeding to shutdown");
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
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

/// **Always returns 200**. See module docs: readiness during decomposition is
/// owned by the controller via the `decomposition.dat6.io/drain` readiness
/// gate, not the app. This is intentional so experiments can attribute
/// improvements to the controller strategy under test.
async fn readyz(State(_state): State<AppState>) -> impl IntoResponse {
    (StatusCode::OK, "ok").into_response()
}

async fn drainez() -> impl IntoResponse {
    let active_connections = IN_FLIGHT_COUNT.load(Ordering::SeqCst);
    let draining = IS_DRAINING.load(Ordering::SeqCst);
    // `ready_to_delete` is meaningful only once the process has actually
    // entered the draining phase — otherwise we'd report `true` for healthy
    // idle pods and the controller would race ahead and remove the finalizer
    // before SIGTERM ever arrives.
    let ready_to_delete = draining && active_connections == 0;
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "active_connections": active_connections,
            "draining": draining,
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

fn rand_simple() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        % 100) as u32
}
