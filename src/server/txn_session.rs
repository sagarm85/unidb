//! Transaction sessions over HTTP (REST enrichment R1): a client-held,
//! multi-request transaction handle.
//!
//! `POST /txn/begin` opens a real engine transaction and registers it here;
//! subsequent requests carry `X-Txn-Id: <xid>` and run their statement under
//! that transaction without auto-committing; `POST /txn/{id}/commit` /
//! `/rollback` finish it. The registry enforces the three hard design points
//! from `docs/backlog/rest_api_enrichment.md`:
//!
//! 1. **In-session serialization.** A single transaction's state (undo log,
//!    snapshot, held locks) is not safe for two concurrent requests on one
//!    `xid`. Each session owns a `tokio::sync::Mutex` taken with `try_lock` —
//!    a second concurrent request on a busy session gets `409 TXN_BUSY`
//!    instead of corrupting the transaction (different sessions still run
//!    fully concurrently).
//! 2. **Idle-session reaper.** An abandoned open transaction holds row locks
//!    and pins the MVCC vacuum horizon (→ bloat). Every session carries an
//!    idle deadline; the background reaper (spawned by [`AppState::new`],
//!    holding only `Weak` references so it never keeps the engine alive)
//!    auto-aborts expired sessions. A dropped client cannot leak a
//!    horizon-pinning transaction.
//! 3. **Principal binding.** A session is bound to the JWT principal (`sub`
//!    claim) that created it; any other principal presenting the `txn_id`
//!    gets `403` — a `txn_id` is not a capability token.
//!
//! Sessions are **ephemeral** (design point 4): recovery aborts in-flight
//! transactions on restart, so a stale `txn_id` simply returns
//! `404 TXN_NOT_FOUND`.
//!
//! [`AppState::new`]: crate::server::AppState::new

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use crate::{format::Xid, txn::IsolationLevel};

/// Why a session checkout failed — mapped to HTTP by `server::error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionError {
    /// No session with that `txn_id` (never existed, already finished, or
    /// reaped) → `404 TXN_NOT_FOUND`.
    NotFound(Xid),
    /// Another request on the same session is still executing →
    /// `409 TXN_BUSY`.
    Busy(Xid),
    /// The session belongs to a different JWT principal → `403`.
    Forbidden(Xid),
}

/// One open transaction session. The engine transaction itself lives in the
/// `TransactionManager`; this is only the HTTP-side handle state.
pub struct TxnSession {
    pub xid: Xid,
    /// The JWT `sub` that created the session (`None` = the implicit
    /// superuser identity of a token without `sub`).
    pub principal: Option<String>,
    pub isolation: IsolationLevel,
    /// In-session serialization (design point 1). `Arc` so a checkout can
    /// hold an owned guard across the blocking engine call.
    busy: Arc<tokio::sync::Mutex<()>>,
    /// Idle clock for the reaper; refreshed when a request finishes.
    last_used: Mutex<Instant>,
}

impl TxnSession {
    fn idle_for(&self) -> Duration {
        self.last_used
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .elapsed()
    }

    fn touch(&self) {
        *self.last_used.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }
}

/// An exclusive checkout of one session for the duration of one request.
/// Dropping it refreshes the idle clock and releases the in-session lock.
pub struct SessionGuard {
    pub session: Arc<TxnSession>,
    _busy: tokio::sync::OwnedMutexGuard<()>,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.session.touch();
    }
}

/// The server-wide session registry.
pub struct TxnSessions {
    inner: Mutex<HashMap<Xid, Arc<TxnSession>>>,
    idle_timeout: Duration,
    /// Lifetime count of sessions the idle reaper has auto-aborted (item 21).
    /// The server-session panel's health signal — a climbing count means
    /// clients are abandoning open transactions (each of which pinned the
    /// vacuum horizon until reaped). Incremented by [`Self::note_reaped`].
    reaper_aborts: AtomicU64,
}

impl TxnSessions {
    pub fn new(idle_timeout: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            idle_timeout,
            reaper_aborts: AtomicU64::new(0),
        }
    }

    pub fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    /// Record that the background reaper auto-aborted an idle session (item 21).
    pub fn note_reaped(&self, n: u64) {
        if n > 0 {
            self.reaper_aborts.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// Lifetime idle-reaper auto-aborts (item 21 server-session panel).
    pub fn reaper_aborts(&self) -> u64 {
        self.reaper_aborts.load(Ordering::Relaxed)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Xid, Arc<TxnSession>>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Register a freshly-begun engine transaction as a session.
    pub fn register(
        &self,
        xid: Xid,
        principal: Option<String>,
        isolation: IsolationLevel,
    ) -> Arc<TxnSession> {
        let session = Arc::new(TxnSession {
            xid,
            principal,
            isolation,
            busy: Arc::new(tokio::sync::Mutex::new(())),
            last_used: Mutex::new(Instant::now()),
        });
        self.lock().insert(xid, session.clone());
        session
    }

    /// Check a session out for one request: verify it exists, verify the
    /// principal, and take the in-session lock (non-blocking). All three
    /// checks happen under the registry lock, so a checkout can never race a
    /// concurrent reap of the same session.
    pub fn checkout(
        &self,
        xid: Xid,
        principal: &Option<String>,
    ) -> Result<SessionGuard, SessionError> {
        let map = self.lock();
        let session = map.get(&xid).ok_or(SessionError::NotFound(xid))?.clone();
        if session.principal != *principal {
            return Err(SessionError::Forbidden(xid));
        }
        let busy = session
            .busy
            .clone()
            .try_lock_owned()
            .map_err(|_| SessionError::Busy(xid))?;
        Ok(SessionGuard {
            session,
            _busy: busy,
        })
    }

    /// Drop a session (after commit/rollback/statement-error/reap). The
    /// caller is responsible for having finished the engine transaction.
    pub fn remove(&self, xid: Xid) {
        self.lock().remove(&xid);
    }

    /// How many sessions are currently open (observability + tests).
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Atomically claim every expired-idle session: each returned session has
    /// been removed from the registry with its busy lock held (so no request
    /// is mid-flight on it and none can start). The caller must abort the
    /// engine transaction for each. Sessions whose busy lock is taken are
    /// skipped — they are actively executing, hence not idle.
    pub fn claim_expired(&self) -> Vec<(Arc<TxnSession>, tokio::sync::OwnedMutexGuard<()>)> {
        let mut map = self.lock();
        let expired: Vec<Xid> = map
            .iter()
            .filter(|(_, s)| s.idle_for() >= self.idle_timeout)
            .map(|(xid, _)| *xid)
            .collect();
        let mut claimed = Vec::new();
        for xid in expired {
            let Some(session) = map.get(&xid).cloned() else {
                continue;
            };
            // Non-blocking: a busy session is mid-request, i.e. not idle.
            let Ok(guard) = session.busy.clone().try_lock_owned() else {
                continue;
            };
            map.remove(&xid);
            claimed.push((session, guard));
        }
        claimed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sessions(idle_ms: u64) -> TxnSessions {
        TxnSessions::new(Duration::from_millis(idle_ms))
    }

    #[test]
    fn checkout_unknown_xid_is_not_found() {
        let reg = sessions(1000);
        assert_eq!(
            reg.checkout(42, &None).err(),
            Some(SessionError::NotFound(42))
        );
    }

    #[test]
    fn concurrent_checkout_of_same_session_is_busy() {
        let reg = sessions(1000);
        reg.register(7, None, IsolationLevel::ReadCommitted);
        let first = reg.checkout(7, &None).expect("first checkout");
        assert_eq!(reg.checkout(7, &None).err(), Some(SessionError::Busy(7)));
        drop(first);
        // Released — a new request can now check the session out.
        assert!(reg.checkout(7, &None).is_ok());
    }

    #[test]
    fn cross_principal_checkout_is_forbidden() {
        let reg = sessions(1000);
        reg.register(7, Some("alice".into()), IsolationLevel::ReadCommitted);
        assert_eq!(
            reg.checkout(7, &Some("bob".into())).err(),
            Some(SessionError::Forbidden(7))
        );
        // An anonymous-superuser token doesn't match a named principal either.
        assert_eq!(
            reg.checkout(7, &None).err(),
            Some(SessionError::Forbidden(7))
        );
        assert!(reg.checkout(7, &Some("alice".into())).is_ok());
    }

    #[test]
    fn claim_expired_takes_idle_sessions_and_skips_busy_ones() {
        let reg = sessions(0); // everything is instantly idle-expired
        reg.register(1, None, IsolationLevel::ReadCommitted);
        reg.register(2, None, IsolationLevel::ReadCommitted);
        let busy = reg.checkout(2, &None).expect("hold session 2 busy");

        let claimed = reg.claim_expired();
        assert_eq!(claimed.len(), 1, "only the idle session is claimed");
        assert_eq!(claimed[0].0.xid, 1);
        // Claimed session is gone from the registry; the busy one survives.
        assert_eq!(
            reg.checkout(1, &None).err(),
            Some(SessionError::NotFound(1))
        );
        drop(busy);
        assert!(reg.checkout(2, &None).is_ok());
    }

    #[test]
    fn touch_on_guard_drop_resets_idle_clock() {
        let reg = sessions(50);
        reg.register(1, None, IsolationLevel::ReadCommitted);
        std::thread::sleep(Duration::from_millis(60));
        // Checking out and dropping refreshes the idle clock…
        drop(reg.checkout(1, &None).expect("checkout"));
        assert!(
            reg.claim_expired().is_empty(),
            "freshly-used session is not idle"
        );
        // …but left alone past the deadline it expires.
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(reg.claim_expired().len(), 1);
    }
}
