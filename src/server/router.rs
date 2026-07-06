//! `build_router` assembles every route onto one `axum::Router`. Data-plane
//! routes live in a `protected` sub-router wrapped with the verify-only JWT
//! middleware (`auth::require_jwt`); `GET /metrics` lives in a separate
//! `public` sub-router that never sees that layer — Prometheus scrapers
//! don't carry app-level bearer tokens (see `auth.rs`'s module doc). Both
//! merge under one top-level `PrometheusMetricLayer` (so `/metrics`
//! requests themselves are counted too) plus `tower-http`'s trace/CORS/
//! timeout middleware.
//!
//! **The `PrometheusMetricLayer`/`PrometheusHandle` pair is a caller-owned
//! argument, not built inside this function.** `PrometheusMetricLayer::
//! pair()` installs a process-global `metrics` recorder — calling it more
//! than once in the same process panics ("Failed to set global recorder").
//! In production (`src/bin/unidb-server.rs`) `build_router` is only ever
//! called once, so this would never matter — but integration tests
//! (M5.d's `tests/server_*.rs`) spin up multiple independent test servers
//! *within one test binary process*. Accepting the pair as an argument
//! lets the test harness obtain it exactly once (e.g. via a `OnceLock`)
//! and reuse it across every test-local server, while production code
//! still gets the natural "call `pair()` once at startup" shape.

use axum::{
    http::StatusCode,
    routing::{delete, get, post},
    Router,
};
use axum_prometheus::{metrics_exporter_prometheus::PrometheusHandle, PrometheusMetricLayer};
use tower_http::{cors::CorsLayer, timeout::TimeoutLayer, trace::TraceLayer};

use crate::server::{auth::JwtConfig, handlers, sse, AppState};

pub fn build_router(
    state: AppState,
    jwt_config: JwtConfig,
    prometheus_layer: PrometheusMetricLayer<'static>,
    metric_handle: PrometheusHandle,
) -> Router {
    let protected = Router::new()
        .route("/txn/begin", post(handlers::post_txn_begin))
        .route("/sql", post(handlers::post_sql))
        .route("/cypher", post(handlers::post_cypher))
        .route("/rows", post(handlers::post_row))
        .route(
            "/rows/{page_id}/{slot}",
            get(handlers::get_row)
                .put(handlers::put_row)
                .delete(handlers::delete_row),
        )
        .route("/edges", post(handlers::post_edge))
        .route("/edges/{page_id}/{slot}", delete(handlers::delete_edge))
        .route("/edges/from/{from_id}", get(handlers::get_edges_from))
        .route("/indexes", post(handlers::post_index))
        .route(
            "/indexes/{table}/{column}/status",
            get(handlers::get_index_status),
        )
        .route("/tables/{table}/events", post(handlers::post_enable_events))
        .route("/events/subscribe", get(sse::get_events_subscribe))
        .route("/events/ack", post(handlers::post_events_ack))
        .route("/checkpoint", post(handlers::post_checkpoint))
        .route_layer(axum::middleware::from_fn_with_state(
            jwt_config,
            crate::server::auth::require_jwt,
        ))
        .with_state(state);

    let public = Router::new().route(
        "/metrics",
        get(move || {
            let handle = metric_handle.clone();
            async move { handle.render() }
        }),
    );

    Router::new()
        .merge(protected)
        .merge(public)
        .layer(prometheus_layer)
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(30),
        ))
}
