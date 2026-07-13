// Shared helper for M5 server integration tests. Not its own test binary —
// Cargo doesn't auto-discover files inside `tests/` subdirectories, so each
// `tests/server_*.rs` file pulls this in via `#[path = "server_common/mod.rs"]
// mod server_common;`. This module is compiled fresh, once per test binary
// that includes it — a binary that doesn't happen to use every helper here
// (e.g. `server_shutdown.rs` doesn't use `TestServer`, `server_crud.rs`
// doesn't use `expired_token`) would otherwise fail `-D warnings`'
// `dead_code` lint; `#![allow(dead_code)]` is the standard, expected fix
// for a shared `tests/*/mod.rs` helper module, not a sign of unused code.
#![cfg(feature = "server")]
#![allow(dead_code)]

use std::{
    net::SocketAddr,
    sync::{Arc, OnceLock},
    time::SystemTime,
};

use axum_prometheus::{metrics_exporter_prometheus::PrometheusHandle, PrometheusMetricLayer};
use jsonwebtoken::{encode, EncodingKey, Header};
use serde::Serialize;
use tempfile::TempDir;
use unidb::server::{
    auth::JwtConfig, engine_handle::EngineHandle, router::build_router, AppState, SessionConfig,
};

pub const TEST_JWT_SECRET: &str = "test-secret-for-unidb-server-integration-tests";

/// `PrometheusMetricLayer::pair()` installs a process-global `metrics`
/// recorder — calling it more than once in the same process panics. Each
/// `tests/server_*.rs` file is its own process (a separate `[[test]]`
/// binary), but a single file's tests all run in that one process, and
/// several call `TestServer::spawn()` — so the pair is obtained exactly
/// once per process and reused across every test-local server, mirroring
/// how `unidb-server`'s own `main()` calls `pair()` exactly once at
/// startup (see `router.rs`'s module doc for the full reasoning).
fn metrics_pair() -> &'static (PrometheusMetricLayer<'static>, PrometheusHandle) {
    static PAIR: OnceLock<(PrometheusMetricLayer<'static>, PrometheusHandle)> = OnceLock::new();
    PAIR.get_or_init(PrometheusMetricLayer::pair)
}

/// A real `unidb-server` bound to an ephemeral port, backed by a fresh
/// temp-dir database. Kept alive for the lifetime of this struct; dropping
/// it aborts the serve task, which drops the last `Arc<EngineHandle>`
/// clone and runs `EngineHandle`'s own bounded-timeout shutdown via `Drop`.
pub struct TestServer {
    pub addr: SocketAddr,
    data_dir: std::path::PathBuf,
    log_dir: std::path::PathBuf,
    _tempdir: TempDir,
    _server_task: tokio::task::JoinHandle<()>,
}

impl TestServer {
    pub async fn spawn() -> Self {
        Self::spawn_with_sessions(SessionConfig::default()).await
    }

    /// [`TestServer::spawn`] with explicit transaction-session / cursor idle
    /// deadlines (R1/R4 tests need short timeouts without touching
    /// process-global env vars).
    pub async fn spawn_with_sessions(config: SessionConfig) -> Self {
        let tempdir = tempfile::tempdir().unwrap();
        let data_dir = tempdir.path().to_path_buf();
        // Point `GET /logs` (item 22) at a dedicated logs subdir so a test can
        // drop synthetic rotated JSON files there without racing the data files.
        let log_dir = data_dir.join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let engine = EngineHandle::spawn(tempdir.path(), 0).unwrap();
        let state = AppState::with_config(Arc::new(engine), config).with_log_dir(log_dir.clone());
        let jwt_config = JwtConfig::new(TEST_JWT_SECRET);
        let (prometheus_layer, metric_handle) = metrics_pair().clone();
        let router = build_router(state, jwt_config, prometheus_layer, metric_handle);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        Self {
            addr,
            data_dir,
            log_dir,
            _tempdir: tempdir,
            _server_task: server_task,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    /// The database directory (holds `audit.log`, `control`, `data.db`, …).
    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// The directory `GET /logs` reads (item 22) — tests drop synthetic rotated
    /// JSON log files here.
    pub fn log_dir(&self) -> &std::path::Path {
        &self.log_dir
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self._server_task.abort();
    }
}

#[derive(Serialize)]
struct Claims {
    sub: String,
    exp: usize,
}

fn now_secs() -> usize {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs() as usize
}

/// A validly-signed, unexpired token for [`TEST_JWT_SECRET`].
pub fn valid_token() -> String {
    token_for("test-user")
}

/// A validly-signed, unexpired token whose `sub` claim is `user` (P6.e per-user
/// identity).
pub fn token_for(user: &str) -> String {
    let claims = Claims {
        sub: user.into(),
        exp: now_secs() + 3600,
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(TEST_JWT_SECRET.as_bytes()),
    )
    .unwrap()
}

/// A validly-structured, correctly-signed-for-the-*wrong*-secret token —
/// exercises signature verification, not just "is this well-formed JSON."
pub fn wrong_signature_token() -> String {
    let claims = Claims {
        sub: "test-user".into(),
        exp: now_secs() + 3600,
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(b"a-completely-different-secret"),
    )
    .unwrap()
}

/// Validly signed for the real secret, but `exp` is in the past.
pub fn expired_token() -> String {
    let claims = Claims {
        sub: "test-user".into(),
        exp: now_secs().saturating_sub(3600),
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(TEST_JWT_SECRET.as_bytes()),
    )
    .unwrap()
}
