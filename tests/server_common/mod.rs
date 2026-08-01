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
    auth::JwtConfig,
    engine_handle::EngineHandle,
    rate_limit::AuthRateLimiter,
    router::{build_router, build_router_with_rate_limiter},
    AppState, SessionConfig,
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
pub fn metrics_pair() -> &'static (PrometheusMetricLayer<'static>, PrometheusHandle) {
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
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await;
        });

        Self {
            addr,
            data_dir,
            log_dir,
            _tempdir: tempdir,
            _server_task: server_task,
        }
    }

    /// Spawn a server with `UNIDB_DEV_LOGIN=1` semantics — `POST /auth/login`
    /// is active and issues tokens signed with `TEST_JWT_SECRET`.
    pub async fn spawn_with_dev_login() -> Self {
        let tempdir = tempfile::tempdir().unwrap();
        let data_dir = tempdir.path().to_path_buf();
        let log_dir = data_dir.join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let engine = EngineHandle::spawn(tempdir.path(), 0).unwrap();
        let jwt_config = JwtConfig::with_dev_login(TEST_JWT_SECRET);
        let state = AppState::with_config(Arc::new(engine), SessionConfig::default())
            .with_log_dir(log_dir.clone())
            .with_dev_login(jwt_config.clone());
        let (prometheus_layer, metric_handle) = metrics_pair().clone();
        let router = build_router(state, jwt_config, prometheus_layer, metric_handle);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await;
        });

        Self {
            addr,
            data_dir,
            log_dir,
            _tempdir: tempdir,
            _server_task: server_task,
        }
    }

    /// [`TestServer::spawn_with_dev_login`] plus an explicit
    /// [`AuthRateLimiter`] (item 121 I1) — used by the rate-limit test
    /// matrix so windows can be milliseconds-short and deterministic instead
    /// of racing real `UNIDB_AUTH_RATE_LIMIT`/`_WINDOW_SECS` env-var defaults
    /// (which are process-global and would otherwise race concurrently
    /// running tests in the same test binary).
    pub async fn spawn_with_dev_login_and_rate_limit(limiter: AuthRateLimiter) -> Self {
        let tempdir = tempfile::tempdir().unwrap();
        let data_dir = tempdir.path().to_path_buf();
        let log_dir = data_dir.join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let engine = EngineHandle::spawn(tempdir.path(), 0).unwrap();
        let jwt_config = JwtConfig::with_dev_login(TEST_JWT_SECRET);
        let state = AppState::with_config(Arc::new(engine), SessionConfig::default())
            .with_log_dir(log_dir.clone())
            .with_dev_login(jwt_config.clone())
            .with_allow_signup(true);
        let (prometheus_layer, metric_handle) = metrics_pair().clone();
        let router = build_router_with_rate_limiter(
            state,
            jwt_config,
            prometheus_layer,
            metric_handle,
            limiter,
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await;
        });

        Self {
            addr,
            data_dir,
            log_dir,
            _tempdir: tempdir,
            _server_task: server_task,
        }
    }

    /// [`TestServer::spawn_with_dev_login`] plus `UNIDB_ALLOW_SIGNUP=1`
    /// semantics (item 121, A3) — `POST /auth/signup` is active.
    pub async fn spawn_with_dev_login_and_signup() -> Self {
        let tempdir = tempfile::tempdir().unwrap();
        let data_dir = tempdir.path().to_path_buf();
        let log_dir = data_dir.join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let engine = EngineHandle::spawn(tempdir.path(), 0).unwrap();
        let jwt_config = JwtConfig::with_dev_login(TEST_JWT_SECRET);
        let state = AppState::with_config(Arc::new(engine), SessionConfig::default())
            .with_log_dir(log_dir.clone())
            .with_dev_login(jwt_config.clone())
            .with_allow_signup(true);
        let (prometheus_layer, metric_handle) = metrics_pair().clone();
        let router = build_router(state, jwt_config, prometheus_layer, metric_handle);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await;
        });

        Self {
            addr,
            data_dir,
            log_dir,
            _tempdir: tempdir,
            _server_task: server_task,
        }
    }

    /// Item 121 A5 — spawn a server with a production signing-key config
    /// (`UNIDB_JWT_SIGNING_KEY` semantics): issuance is enabled via
    /// `JwtConfig::with_signing_key`, independent of `UNIDB_DEV_LOGIN`.
    pub async fn spawn_with_signing_key(secret: &str) -> Self {
        let tempdir = tempfile::tempdir().unwrap();
        let data_dir = tempdir.path().to_path_buf();
        let log_dir = data_dir.join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let engine = EngineHandle::spawn(tempdir.path(), 0).unwrap();
        let jwt_config = JwtConfig::with_signing_key(secret);
        let state = AppState::with_config(Arc::new(engine), SessionConfig::default())
            .with_log_dir(log_dir.clone())
            .with_dev_login(jwt_config.clone());
        let (prometheus_layer, metric_handle) = metrics_pair().clone();
        let router = build_router(state, jwt_config, prometheus_layer, metric_handle);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await;
        });

        Self {
            addr,
            data_dir,
            log_dir,
            _tempdir: tempdir,
            _server_task: server_task,
        }
    }

    /// Item 121 A6 — spawn a server in asymmetric verify-only mode
    /// (`UNIDB_JWT_PUBLIC_KEY` semantics): verification uses the supplied PEM
    /// public key (RSA or EC/P-256, auto-detected); local issuance stays
    /// disabled, matching `unidb-server.rs`'s startup wiring.
    pub async fn spawn_with_asymmetric_public_pem(pem: &[u8]) -> Self {
        let tempdir = tempfile::tempdir().unwrap();
        let data_dir = tempdir.path().to_path_buf();
        let log_dir = data_dir.join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let engine = EngineHandle::spawn(tempdir.path(), 0).unwrap();
        let jwt_config = JwtConfig::from_asymmetric_public_pem(pem)
            .expect("test PEM must parse as a supported RSA or EC public key");
        let state = AppState::with_config(Arc::new(engine), SessionConfig::default())
            .with_log_dir(log_dir.clone());
        let (prometheus_layer, metric_handle) = metrics_pair().clone();
        let router = build_router(state, jwt_config, prometheus_layer, metric_handle);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await;
        });

        Self {
            addr,
            data_dir,
            log_dir,
            _tempdir: tempdir,
            _server_task: server_task,
        }
    }

    /// Item 146 — spawn a server with a production HS256 signing-key config
    /// (`UNIDB_JWT_SIGNING_KEY` semantics, like [`TestServer::spawn_with_signing_key`])
    /// plus an optional previous ("grace") HS256 key
    /// (`UNIDB_JWT_SIGNING_KEY_PREVIOUS` semantics): verification tries
    /// `secret` first, then `previous_secret` when given.
    pub async fn spawn_with_signing_key_and_previous(
        secret: &str,
        previous_secret: Option<&str>,
    ) -> Self {
        let tempdir = tempfile::tempdir().unwrap();
        let data_dir = tempdir.path().to_path_buf();
        let log_dir = data_dir.join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let engine = EngineHandle::spawn(tempdir.path(), 0).unwrap();
        let mut jwt_config = JwtConfig::with_signing_key(secret);
        if let Some(prev) = previous_secret {
            jwt_config = jwt_config.with_previous_hs256_key(prev);
        }
        let state = AppState::with_config(Arc::new(engine), SessionConfig::default())
            .with_log_dir(log_dir.clone())
            .with_dev_login(jwt_config.clone());
        let (prometheus_layer, metric_handle) = metrics_pair().clone();
        let router = build_router(state, jwt_config, prometheus_layer, metric_handle);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await;
        });

        Self {
            addr,
            data_dir,
            log_dir,
            _tempdir: tempdir,
            _server_task: server_task,
        }
    }

    /// Item 146 — spawn a server in asymmetric verify-only mode
    /// (like [`TestServer::spawn_with_asymmetric_public_pem`]) but accepting
    /// either `pem` or `previous_pem`'s matching private key
    /// (`UNIDB_JWT_PUBLIC_KEY_PREVIOUS` semantics).
    pub async fn spawn_with_asymmetric_public_pems(
        pem: &[u8],
        previous_pem: Option<&[u8]>,
    ) -> Self {
        let tempdir = tempfile::tempdir().unwrap();
        let data_dir = tempdir.path().to_path_buf();
        let log_dir = data_dir.join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let engine = EngineHandle::spawn(tempdir.path(), 0).unwrap();
        let jwt_config = JwtConfig::from_asymmetric_public_pems(pem, previous_pem)
            .expect("test PEMs must parse as supported RSA or EC public keys");
        let state = AppState::with_config(Arc::new(engine), SessionConfig::default())
            .with_log_dir(log_dir.clone());
        let (prometheus_layer, metric_handle) = metrics_pair().clone();
        let router = build_router(state, jwt_config, prometheus_layer, metric_handle);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await;
        });

        Self {
            addr,
            data_dir,
            log_dir,
            _tempdir: tempdir,
            _server_task: server_task,
        }
    }

    /// [`TestServer::spawn_with_dev_login`] plus OAuth social-login provider
    /// config (item 128, Workstream D1) — used by the mock-provider OAuth
    /// integration tests to exercise the full authorize→callback flow with
    /// no real network and no real provider secrets.
    pub async fn spawn_with_dev_login_and_oauth(oauth: unidb::server::oauth::OAuthConfig) -> Self {
        let tempdir = tempfile::tempdir().unwrap();
        let data_dir = tempdir.path().to_path_buf();
        let log_dir = data_dir.join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let engine = EngineHandle::spawn(tempdir.path(), 0).unwrap();
        let jwt_config = JwtConfig::with_dev_login(TEST_JWT_SECRET);
        let state = AppState::with_config(Arc::new(engine), SessionConfig::default())
            .with_log_dir(log_dir.clone())
            .with_dev_login(jwt_config.clone())
            .with_oauth(oauth);
        let (prometheus_layer, metric_handle) = metrics_pair().clone();
        let router = build_router(state, jwt_config, prometheus_layer, metric_handle);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await;
        });

        Self {
            addr,
            data_dir,
            log_dir,
            _tempdir: tempdir,
            _server_task: server_task,
        }
    }

    /// [`TestServer::spawn_with_dev_login_and_oauth`], but also (a) returns
    /// the shared `EngineHandle` so a test can seed a vault secret
    /// (`engine.set_secret(...)`) before driving the OAuth flow, and (b)
    /// takes an explicit `master_key` instead of relying on the caller to
    /// twiddle `UNIDB_MASTER_KEY` itself (item 129, Workstream I3).
    ///
    /// `UNIDB_MASTER_KEY` is process-global and read exactly once,
    /// synchronously, inside `EngineHandle::spawn` (`Engine::open` →
    /// `Vault::from_env`) — so the set/open/unset sequence below is guarded
    /// by a static mutex **never held across an `.await`**, keeping it
    /// race-free against any other test in the same `cargo test` binary
    /// that also touches the var (there are none today, but this is the
    /// correct pattern regardless of what else lands in this file later).
    pub async fn spawn_with_dev_login_oauth_and_engine(
        oauth: unidb::server::oauth::OAuthConfig,
        master_key: Option<&str>,
    ) -> (Self, Arc<EngineHandle>) {
        let tempdir = tempfile::tempdir().unwrap();
        let data_dir = tempdir.path().to_path_buf();
        let log_dir = data_dir.join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();

        static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let engine = {
            let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
            match master_key {
                Some(key) => std::env::set_var("UNIDB_MASTER_KEY", key),
                None => std::env::remove_var("UNIDB_MASTER_KEY"),
            }
            let engine = EngineHandle::spawn(tempdir.path(), 0).unwrap();
            std::env::remove_var("UNIDB_MASTER_KEY");
            engine
        };
        let engine = Arc::new(engine);

        let jwt_config = JwtConfig::with_dev_login(TEST_JWT_SECRET);
        let state = AppState::with_config(engine.clone(), SessionConfig::default())
            .with_log_dir(log_dir.clone())
            .with_dev_login(jwt_config.clone())
            .with_oauth(oauth);
        let (prometheus_layer, metric_handle) = metrics_pair().clone();
        let router = build_router(state, jwt_config, prometheus_layer, metric_handle);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await;
        });

        (
            Self {
                addr,
                data_dir,
                log_dir,
                _tempdir: tempdir,
                _server_task: server_task,
            },
            engine,
        )
    }

    /// Item 144 (scheduled jobs / cron) test harness: spawns a normal server
    /// (same shape as [`TestServer::spawn`]) but also returns the shared
    /// `Arc<EngineHandle>` and the `CronState` `AppState::with_config`
    /// constructed for it — the exact pair `tests/item144_cron.rs` needs to
    /// drive the injectable `server::cron::run_due(&engine, &cron, now)`
    /// directly instead of waiting on the real once-a-minute background
    /// loop. Mirrors [`TestServer::spawn_with_dev_login_oauth_and_engine`]'s
    /// "also return the shared handle" precedent.
    pub async fn spawn_with_cron() -> (Self, Arc<EngineHandle>, unidb::server::cron::CronState) {
        let tempdir = tempfile::tempdir().unwrap();
        let data_dir = tempdir.path().to_path_buf();
        let log_dir = data_dir.join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let engine = Arc::new(EngineHandle::spawn(tempdir.path(), 0).unwrap());
        let state = AppState::with_config(engine.clone(), SessionConfig::default())
            .with_log_dir(log_dir.clone());
        let cron_state = state.cron.clone();
        let jwt_config = JwtConfig::new(TEST_JWT_SECRET);
        let (prometheus_layer, metric_handle) = metrics_pair().clone();
        let router = build_router(state, jwt_config, prometheus_layer, metric_handle);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await;
        });

        (
            Self {
                addr,
                data_dir,
                log_dir,
                _tempdir: tempdir,
                _server_task: server_task,
            },
            engine,
            cron_state,
        )
    }

    /// [`TestServer::spawn_with_dev_login_and_signup`] plus an explicit
    /// [`unidb::server::captcha::CaptchaConfig`] (item 131, Workstream I2) —
    /// used by the CAPTCHA integration test matrix. Also returns the shared
    /// `EngineHandle` (mirroring
    /// [`TestServer::spawn_with_dev_login_oauth_and_engine`]) so a test can
    /// seed a vault secret before driving login/signup, with an explicit
    /// `master_key` (see that method's doc comment for the env-guard
    /// rationale).
    pub async fn spawn_with_signup_and_captcha(
        captcha: unidb::server::captcha::CaptchaConfig,
        master_key: Option<&str>,
    ) -> (Self, Arc<EngineHandle>) {
        let tempdir = tempfile::tempdir().unwrap();
        let data_dir = tempdir.path().to_path_buf();
        let log_dir = data_dir.join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();

        static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let engine = {
            let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
            match master_key {
                Some(key) => std::env::set_var("UNIDB_MASTER_KEY", key),
                None => std::env::remove_var("UNIDB_MASTER_KEY"),
            }
            let engine = EngineHandle::spawn(tempdir.path(), 0).unwrap();
            std::env::remove_var("UNIDB_MASTER_KEY");
            engine
        };
        let engine = Arc::new(engine);

        let jwt_config = JwtConfig::with_dev_login(TEST_JWT_SECRET);
        let state = AppState::with_config(engine.clone(), SessionConfig::default())
            .with_log_dir(log_dir.clone())
            .with_dev_login(jwt_config.clone())
            .with_allow_signup(true)
            .with_captcha(captcha);
        let (prometheus_layer, metric_handle) = metrics_pair().clone();
        let router = build_router(state, jwt_config, prometheus_layer, metric_handle);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await;
        });

        (
            Self {
                addr,
                data_dir,
                log_dir,
                _tempdir: tempdir,
                _server_task: server_task,
            },
            engine,
        )
    }

    /// [`TestServer::spawn_with_dev_login_and_signup`] plus an explicit
    /// [`unidb::server::email::EmailConfig`] (item 138) — used by the
    /// password-reset/magic-link integration test matrix. Signup is enabled
    /// too so a test can create the account it then recovers.
    pub async fn spawn_with_dev_login_signup_and_email(
        email: unidb::server::email::EmailConfig,
    ) -> Self {
        let tempdir = tempfile::tempdir().unwrap();
        let data_dir = tempdir.path().to_path_buf();
        let log_dir = data_dir.join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let engine = EngineHandle::spawn(tempdir.path(), 0).unwrap();
        let jwt_config = JwtConfig::with_dev_login(TEST_JWT_SECRET);
        let state = AppState::with_config(Arc::new(engine), SessionConfig::default())
            .with_log_dir(log_dir.clone())
            .with_dev_login(jwt_config.clone())
            .with_allow_signup(true)
            .with_email(email);
        let (prometheus_layer, metric_handle) = metrics_pair().clone();
        let router = build_router(state, jwt_config, prometheus_layer, metric_handle);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await;
        });

        Self {
            addr,
            data_dir,
            log_dir,
            _tempdir: tempdir,
            _server_task: server_task,
        }
    }

    /// [`TestServer::spawn_with_dev_login_signup_and_email`] plus an explicit
    /// [`unidb::server::hibp::HibpConfig`] (item 143, part 1) — used by the
    /// leaked-password integration test matrix. Signup + email are both on
    /// so a single server instance covers signup, admin-create/patch
    /// (superuser bootstrap via `/sql`, same as `item142_auth_admin.rs`),
    /// and password-reset (`POST /auth/verify`) all gated by the same HIBP
    /// config.
    pub async fn spawn_with_signup_email_and_hibp(
        email: unidb::server::email::EmailConfig,
        hibp: unidb::server::hibp::HibpConfig,
    ) -> Self {
        let tempdir = tempfile::tempdir().unwrap();
        let data_dir = tempdir.path().to_path_buf();
        let log_dir = data_dir.join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let engine = EngineHandle::spawn(tempdir.path(), 0).unwrap();
        let jwt_config = JwtConfig::with_dev_login(TEST_JWT_SECRET);
        let state = AppState::with_config(Arc::new(engine), SessionConfig::default())
            .with_log_dir(log_dir.clone())
            .with_dev_login(jwt_config.clone())
            .with_allow_signup(true)
            .with_email(email)
            .with_hibp(hibp);
        let (prometheus_layer, metric_handle) = metrics_pair().clone();
        let router = build_router(state, jwt_config, prometheus_layer, metric_handle);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await;
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

/// A validly-signed, unexpired token carrying `sub` (optional) plus every
/// `(key, value)` in `extra` — item 122 (B2/B3/B4: `auth.jwt()`/`role`
/// claims) and item 124 (E1: realtime per-subscriber RLS) tests need custom
/// claims (`tenant`, `role: "service_role"`, …) beyond the plain `sub`/`exp`
/// shape [`Claims`] covers. Encodes a raw `serde_json::Value` object rather
/// than a fixed struct so any claim shape a test needs is expressible.
pub fn token_with_claims(sub: Option<&str>, extra: &[(&str, serde_json::Value)]) -> String {
    let mut map = serde_json::Map::new();
    if let Some(s) = sub {
        map.insert("sub".to_string(), serde_json::Value::String(s.to_string()));
    }
    map.insert("exp".to_string(), serde_json::json!(now_secs() + 3600));
    for (k, v) in extra {
        map.insert((*k).to_string(), v.clone());
    }
    let claims = serde_json::Value::Object(map);
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(TEST_JWT_SECRET.as_bytes()),
    )
    .unwrap()
}
