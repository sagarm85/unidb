// Shared test helper for unidb-attach integration tests.  Each test file
// pulls this in via:
//   #[path = "attach_common/mod.rs"]
//   mod attach_common;
//
// `#![allow(dead_code)]` is expected for a shared helper module — not every
// test file uses every helper.
#![allow(dead_code)]

use std::sync::{Arc, OnceLock};

use axum_prometheus::{metrics_exporter_prometheus::PrometheusHandle, PrometheusMetricLayer};
use jsonwebtoken::{encode, EncodingKey, Header};
use serde::Serialize;
use tempfile::TempDir;
use unidb::server::{auth::JwtConfig, engine_handle::EngineHandle, router::build_router, AppState};

pub const JWT_SECRET: &str = "unidb-attach-integration-test-secret";

// `PrometheusMetricLayer::pair()` installs a process-global recorder —
// calling it more than once per process panics.  Each test *file* is a
// separate binary process, so we use OnceLock to call pair() exactly once
// per process (i.e. once per file's tests, even if they call spawn() many times).
fn metrics_pair() -> &'static (PrometheusMetricLayer<'static>, PrometheusHandle) {
    static PAIR: OnceLock<(PrometheusMetricLayer<'static>, PrometheusHandle)> = OnceLock::new();
    PAIR.get_or_init(PrometheusMetricLayer::pair)
}

/// A real `unidb-server` on an ephemeral port, backed by a fresh temp-dir.
///
/// `AttachClient` is blocking (`reqwest::blocking`), so each test function
/// must be a plain `#[test]`, NOT `#[tokio::test]`.  The Tokio runtime for
/// the server is started inside `spawn()` and is held inside this struct.
pub struct TestServer {
    pub base_url: String,
    // `rt` is `Option` so we can call `shutdown_background(self)` (which
    // takes ownership) in `Drop::drop(&mut self)`.  It is `None` only during
    // `drop` itself.
    rt: Option<tokio::runtime::Runtime>,
    // `_tempdir` declared last so the directory outlives the runtime (fields
    // are dropped in declaration order); the engine releases its file handles
    // during runtime shutdown before the directory is deleted.
    _tempdir: TempDir,
}

impl TestServer {
    pub fn spawn() -> Self {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let tempdir = tempfile::tempdir().unwrap();
        let dir_path = tempdir.path().to_path_buf();

        let base_url = rt.block_on(async move {
            let engine = EngineHandle::spawn(&dir_path, 0).unwrap();
            let state = AppState::new(Arc::new(engine));
            let jwt_config = JwtConfig::new(JWT_SECRET);
            let (layer, handle) = metrics_pair().clone();
            let router = build_router(state, jwt_config, layer, handle);

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let _ = axum::serve(listener, router).await;
            });
            format!("http://{addr}")
        });

        Self {
            base_url,
            rt: Some(rt),
            _tempdir: tempdir,
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        // `shutdown_background` takes `self` by value, so we `take()` the
        // Option to get ownership — the Option is None only here in drop,
        // which is fine since no code runs after drop returns.
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

// ── JWT helpers ───────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct Claims {
    sub: String,
    exp: usize,
}

fn now_secs() -> usize {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as usize
}

/// A validly-signed, unexpired token for `JWT_SECRET`.
pub fn valid_token() -> String {
    encode(
        &Header::default(),
        &Claims {
            sub: "test-user".into(),
            exp: now_secs() + 3600,
        },
        &EncodingKey::from_secret(JWT_SECRET.as_bytes()),
    )
    .unwrap()
}
