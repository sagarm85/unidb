//! `unidb-server-full`: unidb-server with storage wired in (item 31).
//! Accepts all the same env vars as `unidb-server`, plus:
//!   STORAGE_BACKEND=minio|s3|memory  — enables storage; omit to disable (503)
//!   STORAGE_ENDPOINT / STORAGE_S3_ENDPOINT
//!   STORAGE_ACCESS_KEY / STORAGE_SECRET_KEY
//!   STORAGE_BUCKET                   — physical MinIO/S3 bucket (default "unidb")

use std::sync::Arc;

use axum_prometheus::PrometheusMetricLayer;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use unidb::server::{auth::JwtConfig, engine_handle::EngineHandle, router::build_router, AppState};
use unidb::storage_api::StorageApi;
use unidb_storage::{ObjectStore, S3ObjectStore, StorageConfig, StorageService};

fn init_logging(log_dir: &str) -> tracing_appender::non_blocking::WorkerGuard {
    std::fs::create_dir_all(log_dir)
        .unwrap_or_else(|e| panic!("failed to create log dir {log_dir}: {e}"));
    let (file_writer, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily(log_dir, "unidb.log"));

    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let json = std::env::var("UNIDB_LOG_FORMAT")
        .map(|v| !v.eq_ignore_ascii_case("text"))
        .unwrap_or(true);

    if json {
        tracing_subscriber::registry()
            .with(filter())
            .with(fmt::layer().json())
            .with(
                fmt::layer()
                    .json()
                    .with_writer(file_writer)
                    .with_ansi(false),
            )
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter())
            .with(fmt::layer())
            .with(fmt::layer().with_writer(file_writer).with_ansi(false))
            .init();
    }
    guard
}

/// Try to build a live StorageService from env. Returns None on any failure so
/// the server still boots — all /storage/* routes then return 503.
async fn try_init_storage(engine_arc: Arc<unidb::Engine>) -> Option<Arc<dyn StorageApi>> {
    // Only activate when STORAGE_BACKEND is explicitly set.
    if std::env::var("STORAGE_BACKEND").is_err() {
        tracing::info!("STORAGE_BACKEND not set; /storage/* returns 503");
        return None;
    }

    let cfg = match StorageConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("storage config error ({e}); /storage/* returns 503");
            return None;
        }
    };

    let store: Arc<dyn ObjectStore> = match S3ObjectStore::from_config(&cfg) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::warn!("storage store init failed ({e}); /storage/* returns 503");
            return None;
        }
    };

    match StorageService::new(engine_arc, store, cfg).await {
        Ok(svc) => {
            tracing::info!(
                backend = %std::env::var("STORAGE_BACKEND").unwrap_or_default(),
                "storage service ready"
            );
            Some(Arc::new(svc) as Arc<dyn StorageApi>)
        }
        Err(e) => {
            tracing::warn!("storage init failed ({e}); /storage/* returns 503");
            None
        }
    }
}

#[tokio::main]
async fn main() {
    let data_dir = std::env::var("UNIDB_DATA_DIR").unwrap_or_else(|_| "/tmp/unidb".to_string());
    let log_dir = std::env::var("UNIDB_LOG_DIR").unwrap_or_else(|_| format!("{data_dir}/logs"));
    let _log_guard = init_logging(&log_dir);

    let page_size: u32 = std::env::var("UNIDB_PAGE_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let bind_addr =
        std::env::var("UNIDB_BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let jwt_secret = std::env::var("UNIDB_JWT_SECRET").expect("UNIDB_JWT_SECRET must be set");

    let engine_handle = EngineHandle::spawn(std::path::Path::new(&data_dir), page_size)
        .unwrap_or_else(|e| panic!("failed to open unidb at {data_dir}: {e}"));
    let engine_handle = Arc::new(engine_handle);

    let engine_arc = engine_handle
        .engine_arc()
        .expect("engine available at startup");
    let storage = try_init_storage(engine_arc).await;

    // item 100: UNIDB_DEV_LOGIN=1 activates POST /auth/login (dev/demo only —
    // mirrors src/bin/unidb-server.rs; this binary never wired it up). Two
    // separate things both need it: the `jwt_config` passed to build_router
    // (verify middleware) AND AppState's own dev_login_jwt field (what the
    // /auth/meta and /auth/login handlers actually read) — missing either
    // one leaves dev_login_enabled false even with the env var set.
    let dev_login = std::env::var("UNIDB_DEV_LOGIN")
        .ok()
        .map(|v| v == "1" || v.to_ascii_lowercase() == "true")
        .unwrap_or(false);
    if dev_login {
        tracing::warn!(
            "UNIDB_DEV_LOGIN=1: POST /auth/login is enabled (passwordless, dev/demo only — \
             do NOT use in production)"
        );
    }
    let jwt_config = if dev_login {
        JwtConfig::with_dev_login(&jwt_secret)
    } else {
        JwtConfig::new(&jwt_secret)
    };

    let state = {
        let s = AppState::new(engine_handle)
            .with_log_dir(std::path::PathBuf::from(&log_dir))
            .with_storage(storage);
        if dev_login {
            s.with_dev_login(jwt_config.clone())
        } else {
            s
        }
    };
    let (prometheus_layer, metric_handle) = PrometheusMetricLayer::pair();
    let router = build_router(state, jwt_config, prometheus_layer, metric_handle);

    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {bind_addr}: {e}"));
    tracing::info!(addr = %bind_addr, data_dir = %data_dir, "unidb-server-full listening");
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("shutdown signal; draining requests");
        })
        .await
        .expect("server error");
}
