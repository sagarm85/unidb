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
//! route), [`router`] (`build_router`), [`auth`] (verify-only JWT
//! middleware), [`sse`] (`GET /events/subscribe`), [`txn_session`]
//! (multi-request transaction sessions, R1), [`cursor`] (large-result
//! pagination, R4). `/metrics` (Prometheus, via `axum-prometheus`) is wired
//! directly in `router.rs` rather than its own module — there's no reusable
//! logic beyond one `PrometheusMetricLayer::pair()` call.

pub mod auth;
pub mod cursor;
pub mod dto;
pub mod engine_handle;
pub mod error;
pub mod handlers;
pub mod router;
pub mod sse;
pub mod tls;
pub mod txn_session;

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
/// registries. Cloning per-request is cheap (three `Arc`s).
#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<EngineHandle>,
    pub sessions: Arc<TxnSessions>,
    pub cursors: Arc<CursorStore>,
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
        }
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
            for (session, _busy) in sessions.claim_expired() {
                match engine.abort(session.xid).await {
                    Ok(()) => tracing::info!(
                        xid = session.xid,
                        "auto-aborted idle transaction session (reaper)"
                    ),
                    Err(e) => tracing::warn!(
                        xid = session.xid,
                        error = %e,
                        "failed to abort idle transaction session"
                    ),
                }
            }
        }
    });
}
