//! Optional REST/JWT/SSE/metrics server (M5), gated behind the `server`
//! Cargo feature so a default `cargo build`/`cargo test` of the embedded
//! crate never depends on tokio/axum/etc. — see `lib.rs`'s crate doc and
//! `CLAUDE.md`'s "tokio (M5 server only — the engine stays sync)" note.
//!
//! **Concurrency shape (P5.e-3):** `Engine` is `Send + Sync`, so
//! [`engine_handle::EngineHandle`] holds one shared `Arc<Engine>` and runs
//! each blocking engine call on a tokio blocking-pool thread via
//! `spawn_blocking` — many requests execute in parallel across cores,
//! coordinating only through the engine's internal latches/locks. (The
//! original M5 design funneled every write through one dedicated writer
//! thread; that shape was retired when the engine became `Sync`.)
//!
//! Submodules: [`engine_handle`] (the `Arc<Engine>`/`spawn_blocking`
//! bridge), [`error`] (`DbError` → HTTP status mapping), [`dto`]
//! (wire-format request/response shapes), [`handlers`] (one `async fn` per
//! route), [`rest_resource`] (item 123: the schema-derived `/rest/v1/*`
//! auto REST API — translates query params into parameterized SQL run
//! through the same enforced path as `POST /sql`), [`router`]
//! (`build_router`), [`auth`] (verify-only JWT middleware), [`oauth`] (item
//! 128: OAuth 2.0 Authorization Code + PKCE provider config/HTTP calls for
//! `GET /auth/oauth/<provider>/authorize`/`callback`), [`sse`]
//! (`GET /events/subscribe`), [`txn_session`] (multi-request transaction
//! sessions, R1), [`cursor`] (large-result pagination, R4). `/metrics`
//! (Prometheus, via `axum-prometheus`) is wired directly in `router.rs`
//! rather than its own module — there's no reusable logic beyond one
//! `PrometheusMetricLayer::pair()` call.

pub mod auth;
pub mod bulk;
pub mod correlation;
pub mod cursor;
pub mod dto;
pub mod engine_handle;
pub mod error;
pub mod event_format;
pub mod handlers;
pub mod logs;
pub mod oauth;
pub mod rate_limit;
pub mod rest_resource;
pub mod router;
pub mod sse;
pub mod storage;
pub mod tls;
pub mod txn_session;

use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Duration;

use cursor::CursorStore;
use engine_handle::EngineHandle;
use txn_session::TxnSessions;

/// Idle deadlines for transaction sessions (R1) and result cursors (R4).
#[derive(Debug, Clone, Copy)]
pub struct SessionConfig {
    /// A transaction session idle longer than this is auto-aborted by the
    /// reaper (it holds row locks and pins the MVCC vacuum horizon — an
    /// abandoned one must not leak). `UNIDB_TXN_IDLE_TIMEOUT_SECS`, default 60.
    pub txn_idle_timeout: Duration,
    /// A result cursor idle longer than this is dropped.
    /// `UNIDB_CURSOR_IDLE_TIMEOUT_SECS`, default 60.
    pub cursor_idle_timeout: Duration,
}

impl SessionConfig {
    pub fn from_env() -> Self {
        fn env_secs(var: &str, default: u64) -> Duration {
            Duration::from_secs(
                std::env::var(var)
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(default),
            )
        }
        Self {
            txn_idle_timeout: env_secs("UNIDB_TXN_IDLE_TIMEOUT_SECS", 60),
            cursor_idle_timeout: env_secs("UNIDB_CURSOR_IDLE_TIMEOUT_SECS", 60),
        }
    }
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            txn_idle_timeout: Duration::from_secs(60),
            cursor_idle_timeout: Duration::from_secs(60),
        }
    }
}

/// Shared state threaded through every handler via axum's `State`
/// extractor: the engine bridge plus the transaction-session and cursor
/// registries. Cloning per-request is cheap (four `Arc`s).
#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<EngineHandle>,
    pub sessions: Arc<TxnSessions>,
    pub cursors: Arc<CursorStore>,
    /// Directory the rolling JSON log files live in — the source `GET /logs`
    /// (item 22, L3) reads. Defaults from `UNIDB_LOG_DIR` (mirroring
    /// `unidb-server`'s own resolution) so it points at the same files the
    /// server is writing.
    pub log_dir: Arc<PathBuf>,
    /// Item 31: optional storage service. `None` when `STORAGE_BACKEND` is not
    /// set or when init failed at startup (graceful degradation — server boots
    /// cleanly without storage). All `/storage/*` handlers return 503 when this
    /// is `None`. Held as `dyn StorageApi` so `unidb` need not depend on
    /// `unidb-storage` (which already depends on `unidb`) — no crate cycle.
    pub storage: Option<std::sync::Arc<dyn crate::storage_api::StorageApi>>,
    /// Non-None when `UNIDB_DEV_LOGIN=1` — the JWT secret is stored here so
    /// `POST /auth/login` can issue tokens.  None = login disabled (production
    /// default; Milestone-18 "verify-only" stays intact).
    pub dev_login_jwt: Option<auth::JwtConfig>,
    /// `UNIDB_ALLOW_SIGNUP=1` (item 121, A3) — activates `POST /auth/signup`.
    /// `false` by default (opt-in, not open by default); when `false` the
    /// route returns 404, indistinguishable from a non-existent route, same
    /// posture as `dev_login_jwt` being `None`.
    pub allow_signup: bool,
    /// OAuth 2.0 social-login provider config (item 128, Workstream D1).
    /// Empty by default — every `/auth/oauth/<provider>/*` route 404s for a
    /// provider not present here, same "off by default, safe with zero
    /// config" posture as `dev_login_jwt`/`allow_signup`.
    pub oauth: oauth::OAuthConfig,
}

/// Resolve the log directory the same way `src/bin/unidb-server.rs` does, so
/// `GET /logs` reads exactly the files being written (`UNIDB_LOG_DIR`, else
/// `<UNIDB_DATA_DIR>/logs`).
fn default_log_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("UNIDB_LOG_DIR") {
        return PathBuf::from(dir);
    }
    let data_dir = std::env::var("UNIDB_DATA_DIR").unwrap_or_else(|_| "/tmp/unidb".to_string());
    PathBuf::from(format!("{data_dir}/logs"))
}

impl AppState {
    /// Build the state and spawn the background session/cursor reaper.
    /// Deadlines come from the environment (see [`SessionConfig::from_env`]).
    /// Must be called inside a tokio runtime.
    pub fn new(engine: Arc<EngineHandle>) -> Self {
        Self::with_config(engine, SessionConfig::from_env())
    }

    /// [`AppState::new`] with explicit deadlines — used by tests that need
    /// short idle timeouts without touching process-global env vars.
    pub fn with_config(engine: Arc<EngineHandle>, config: SessionConfig) -> Self {
        let sessions = Arc::new(TxnSessions::new(config.txn_idle_timeout));
        let cursors = Arc::new(CursorStore::new(config.cursor_idle_timeout));
        spawn_reaper(
            Arc::downgrade(&engine),
            Arc::downgrade(&sessions),
            Arc::downgrade(&cursors),
        );
        Self {
            engine,
            sessions,
            cursors,
            log_dir: Arc::new(default_log_dir()),
            storage: None,
            dev_login_jwt: None,
            allow_signup: false,
            oauth: oauth::OAuthConfig::empty(),
        }
    }

    /// Activate dev-only login (`UNIDB_DEV_LOGIN=1`).  See `auth::JwtConfig::with_dev_login`.
    pub fn with_dev_login(mut self, jwt: auth::JwtConfig) -> Self {
        self.dev_login_jwt = Some(jwt);
        self
    }

    /// Activate `POST /auth/signup` (`UNIDB_ALLOW_SIGNUP=1`, item 121, A3).
    pub fn with_allow_signup(mut self, allow: bool) -> Self {
        self.allow_signup = allow;
        self
    }

    /// Configure OAuth social-login providers (item 128, Workstream D1).
    /// An empty [`oauth::OAuthConfig`] (the default) means every
    /// `/auth/oauth/*` route 404s.
    pub fn with_oauth(mut self, oauth: oauth::OAuthConfig) -> Self {
        self.oauth = oauth;
        self
    }

    /// Point `GET /logs` at an explicit log directory (the server binary passes
    /// its resolved `UNIDB_LOG_DIR`; tests point it at a temp dir).
    pub fn with_log_dir(mut self, dir: PathBuf) -> Self {
        self.log_dir = Arc::new(dir);
        self
    }

    /// Attach a storage service (item 31). Pass `None` when storage is not
    /// configured; all `/storage/*` routes return 503 in that case.
    pub fn with_storage(
        mut self,
        svc: Option<std::sync::Arc<dyn crate::storage_api::StorageApi>>,
    ) -> Self {
        self.storage = svc;
        self
    }
}

/// Background reaper for idle transaction sessions and cursors (R1 design
/// point 2 — non-negotiable: a dropped client must not leak a
/// horizon-pinning transaction). Holds only `Weak` references, so it never
/// keeps the engine (or the registries) alive: when the server's `AppState`
/// is dropped, the next tick fails to upgrade and the task exits.
fn spawn_reaper(
    engine: Weak<EngineHandle>,
    sessions: Weak<TxnSessions>,
    cursors: Weak<CursorStore>,
) {
    // Tick fast enough that a short test deadline is honored promptly, but
    // never busier than 20 Hz.
    let tick = {
        let shortest = sessions
            .upgrade()
            .map(|s| s.idle_timeout())
            .unwrap_or(Duration::from_secs(60))
            .min(
                cursors
                    .upgrade()
                    .map(|c| c.idle_timeout())
                    .unwrap_or(Duration::from_secs(60)),
            );
        (shortest / 4).clamp(Duration::from_millis(50), Duration::from_secs(2))
    };
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tick).await;
            let (Some(engine), Some(sessions), Some(cursors)) =
                (engine.upgrade(), sessions.upgrade(), cursors.upgrade())
            else {
                return; // server state dropped — nothing left to reap
            };
            let swept = cursors.sweep();
            if swept > 0 {
                tracing::debug!(swept, "reaped idle result cursors");
            }
            // Each claimed session was removed from the registry with its
            // busy lock held, so no request is (or can start) mid-flight on
            // it; aborting releases its row locks and un-pins the vacuum
            // horizon.
            let mut reaped = 0u64;
            for (session, _busy) in sessions.claim_expired() {
                match engine.abort(session.xid).await {
                    Ok(()) => {
                        reaped += 1;
                        tracing::info!(
                            xid = session.xid,
                            "auto-aborted idle transaction session (reaper)"
                        )
                    }
                    Err(e) => tracing::warn!(
                        xid = session.xid,
                        error = %e,
                        "failed to abort idle transaction session"
                    ),
                }
            }
            // item 21: surface abandoned-transaction churn on the session panel.
            sessions.note_reaped(reaped);
        }
    });
}
