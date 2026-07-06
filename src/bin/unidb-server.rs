//! `unidb-server`: the optional REST/JWT/SSE/metrics server binary (M5).
//! Config comes from environment variables (no config file in v1):
//! - `UNIDB_DATA_DIR` (default `./unidb-data`): directory `Engine::open`s.
//! - `UNIDB_PAGE_SIZE` (default `0`, meaning `Engine::open`'s own default).
//! - `UNIDB_BIND_ADDR` (default `127.0.0.1:8080`).
//! - `UNIDB_JWT_SECRET` (**required**): HMAC secret for verify-only JWT
//!   auth (`server::auth`). No default — refusing to start without an
//!   explicit secret is safer than silently running unauthenticated.
//!
//! Startup order: open the `Engine` via `EngineHandle::spawn` (synchronous,
//! surfaces any open/recovery failure immediately — see `engine_handle.rs`'s
//! module doc), build the router, bind, serve. `axum::serve`'s graceful
//! shutdown waits for in-flight requests to finish on `Ctrl-C`; once it
//! returns, every request-scoped `Arc<EngineHandle>` clone has already been
//! dropped, so the last reference (held here in `main`) drops naturally at
//! the end of this function, running `EngineHandle`'s `Drop` — which sends
//! `Shutdown` and joins the writer thread within its bounded timeout,
//! mirroring `IndexHandle`'s own shutdown-on-drop precedent.

use std::sync::Arc;

use axum_prometheus::PrometheusMetricLayer;
use unidb::server::{auth::JwtConfig, engine_handle::EngineHandle, router::build_router, AppState};

#[tokio::main]
async fn main() {
    unidb::init_tracing();

    let data_dir = std::env::var("UNIDB_DATA_DIR").unwrap_or_else(|_| "./unidb-data".to_string());
    let page_size: u32 = std::env::var("UNIDB_PAGE_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let bind_addr =
        std::env::var("UNIDB_BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let jwt_secret = std::env::var("UNIDB_JWT_SECRET")
        .expect("UNIDB_JWT_SECRET must be set (verify-only JWT auth has no default secret)");

    let engine = EngineHandle::spawn(std::path::Path::new(&data_dir), page_size)
        .unwrap_or_else(|e| panic!("failed to open unidb engine at {data_dir}: {e}"));
    let state = AppState {
        engine: Arc::new(engine),
    };
    let jwt_config = JwtConfig::new(&jwt_secret);
    let (prometheus_layer, metric_handle) = PrometheusMetricLayer::pair();

    let router = build_router(state, jwt_config, prometheus_layer, metric_handle);
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {bind_addr}: {e}"));
    tracing::info!(addr = %bind_addr, data_dir = %data_dir, "unidb-server listening");

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");

    tracing::info!("unidb-server shut down cleanly");
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install Ctrl-C handler");
    tracing::info!("shutdown signal received, draining in-flight requests");
}
