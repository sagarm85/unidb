//! `unidb-server`: the optional REST/JWT/SSE/metrics server binary (M5).
//! Config comes from environment variables (no config file in v1):
//! - `UNIDB_DATA_DIR` (default `./unidb-data`): directory `Engine::open`s —
//!   holds `control`/`data.db`/`db.wal`, nothing else.
//! - `UNIDB_LOG_DIR` (default `<UNIDB_DATA_DIR>/logs`): directory for
//!   rolling daily log files (`unidb.log.YYYY-MM-DD`). Independently
//!   overridable so a deployment can put logs on a different volume than
//!   data (a common ops pattern — e.g. a smaller, faster disk for data,
//!   a larger/shared one for logs) while still defaulting to one
//!   self-contained folder for local/dev use.
//! - `UNIDB_PAGE_SIZE` (default `0`, meaning `Engine::open`'s own default).
//! - `UNIDB_BIND_ADDR` (default `127.0.0.1:8080`).
//! - `UNIDB_JWT_SECRET` (**required**): HMAC secret for verify-only JWT
//!   auth (`server::auth`). No default — refusing to start without an
//!   explicit secret is safer than silently running unauthenticated.
//!
//! Logging goes to **both** stdout (so `docker logs`/interactive/systemd
//! journal capture still works unchanged) and a rolling daily file under
//! `UNIDB_LOG_DIR` — unlike the embedded library's plain `unidb::
//! init_tracing()` (stdout-only, meant for an app that manages its own
//! logging), a long-running server process needs a persistent log a
//! restarted process or log-shipping agent can actually find. This is
//! deliberately implemented here rather than in `lib.rs` so the default,
//! non-`server` embedded build stays untouched.
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
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use unidb::server::{auth::JwtConfig, engine_handle::EngineHandle, router::build_router, AppState};

/// Sets up dual stdout+file logging and returns the file appender's guard —
/// the caller must keep it alive for the process lifetime (dropping it
/// early silently stops flushing buffered log lines to the file, since
/// `tracing-appender`'s non-blocking writer flushes on `Drop`).
fn init_logging(log_dir: &str) -> tracing_appender::non_blocking::WorkerGuard {
    std::fs::create_dir_all(log_dir)
        .unwrap_or_else(|e| panic!("failed to create log directory {log_dir}: {e}"));
    let file_appender = tracing_appender::rolling::daily(log_dir, "unidb.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let env_filter =
        || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(env_filter())
        .with(fmt::layer())
        .with(fmt::layer().with_writer(file_writer).with_ansi(false))
        .init();
    guard
}

#[tokio::main]
async fn main() {
    let data_dir = std::env::var("UNIDB_DATA_DIR").unwrap_or_else(|_| "./unidb-data".to_string());
    let log_dir = std::env::var("UNIDB_LOG_DIR").unwrap_or_else(|_| format!("{data_dir}/logs"));
    // Held for the whole process lifetime — see `init_logging`'s doc comment.
    let _log_guard = init_logging(&log_dir);

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

    // P6.f: serve HTTPS directly when TLS is configured; otherwise plain HTTP.
    if let Some((cert, key)) = unidb::server::tls::tls_paths_from_env() {
        unidb::server::tls::install_crypto_provider();
        let config = unidb::server::tls::load_rustls_config(cert.as_ref(), key.as_ref())
            .await
            .unwrap_or_else(|e| panic!("failed to load TLS cert/key: {e}"));
        let addr: std::net::SocketAddr = bind_addr
            .parse()
            .unwrap_or_else(|e| panic!("invalid UNIDB_BIND_ADDR {bind_addr}: {e}"));
        tracing::info!(addr = %bind_addr, data_dir = %data_dir, log_dir = %log_dir, tls = true, "unidb-server listening (HTTPS)");
        axum_server::bind_rustls(addr, config)
            .serve(router.into_make_service())
            .await
            .expect("server error");
    } else {
        let listener = tokio::net::TcpListener::bind(&bind_addr)
            .await
            .unwrap_or_else(|e| panic!("failed to bind {bind_addr}: {e}"));
        tracing::info!(addr = %bind_addr, data_dir = %data_dir, log_dir = %log_dir, tls = false, "unidb-server listening (HTTP)");
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .expect("server error");
    }

    tracing::info!("unidb-server shut down cleanly");
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install Ctrl-C handler");
    tracing::info!("shutdown signal received, draining in-flight requests");
}
