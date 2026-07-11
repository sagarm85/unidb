//! P5.f resource control: per-query **timeout**, **cancellation**, and
//! **`work_mem`** (spill budget). A query now runs on one worker thread (P5.e-3),
//! so its limits live in a **thread-local** set for the duration of the call —
//! no need to thread a context object through every executor function. The hot
//! loops call [`check`] at row/batch granularity, and the spill operators read
//! [`work_mem_rows`] instead of a fixed constant.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::{DbError, Result};

/// A shared, cheaply-cloneable cancellation flag. Set it from any thread (e.g.
/// a request handler on client disconnect) to make the running query stop with
/// [`DbError::QueryCancelled`] at its next [`check`] point.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }
    /// Request cancellation. Idempotent; observable from any thread.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Per-query resource limits. All fields are optional — an unset field imposes
/// no limit (the pre-P5.f behavior), so `QueryLimits::default()` is a no-op.
#[derive(Clone, Debug, Default)]
pub struct QueryLimits {
    /// Absolute wall-clock deadline; the query aborts once `Instant::now()`
    /// passes it. Derived from a caller-supplied `timeout` duration.
    pub deadline: Option<Instant>,
    /// Cooperative cancellation flag.
    pub cancel: Option<CancelToken>,
    /// `work_mem` expressed as an in-memory row budget for the spill operators
    /// (`ORDER BY` external sort, hash-join Grace spill). Overrides the process
    /// default / `UNIDB_*_MEM_ROWS` env vars for this query only.
    pub work_mem_rows: Option<usize>,
    /// The original timeout, kept only so [`check`] can report `limit_ms`.
    timeout: Option<Duration>,
}

impl QueryLimits {
    /// Limits with just a timeout.
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            deadline: Some(Instant::now() + timeout),
            timeout: Some(timeout),
            ..Default::default()
        }
    }
    pub fn set_cancel(mut self, token: CancelToken) -> Self {
        self.cancel = Some(token);
        self
    }
    pub fn set_work_mem_rows(mut self, rows: usize) -> Self {
        self.work_mem_rows = Some(rows);
        self
    }
}

thread_local! {
    static CURRENT: RefCell<Option<QueryLimits>> = const { RefCell::new(None) };
}

/// RAII guard that installs `limits` as the current thread's query limits and
/// restores the previous value (usually `None`) on drop — so nested/reentrant
/// calls and early returns can't leak a stale deadline onto a pooled thread.
pub struct LimitsGuard(Option<QueryLimits>);

impl Drop for LimitsGuard {
    fn drop(&mut self) {
        CURRENT.with(|c| *c.borrow_mut() = self.0.take());
    }
}

/// Install `limits` for the current thread until the returned guard drops.
#[must_use = "hold the guard for the duration of the query"]
pub fn install(limits: QueryLimits) -> LimitsGuard {
    let prev = CURRENT.with(|c| c.borrow_mut().replace(limits));
    LimitsGuard(prev)
}

/// Abort the running query if its deadline has passed or its cancel flag is set.
/// Cheap (a thread-local read + at most an `Instant::now`) — safe to call in an
/// inner loop, though callers batch it (every N rows) to keep it truly free.
pub fn check() -> Result<()> {
    CURRENT.with(|c| {
        let b = c.borrow();
        let Some(limits) = b.as_ref() else {
            return Ok(());
        };
        if let Some(cancel) = &limits.cancel {
            if cancel.is_cancelled() {
                return Err(DbError::QueryCancelled);
            }
        }
        if let Some(deadline) = limits.deadline {
            if Instant::now() >= deadline {
                let limit_ms = limits.timeout.map(|d| d.as_millis() as u64).unwrap_or(0);
                return Err(DbError::QueryTimeout { limit_ms });
            }
        }
        Ok(())
    })
}

/// A `Send + Sync` snapshot of the current query's deadline + cancel token, so a
/// **parallel-scan worker** (running on a *different* thread, without the
/// thread-local) can honor the query's timeout/cancellation exactly like
/// [`check`] does on the query thread. Cheap; a no-op if no limits are installed.
#[derive(Clone, Debug, Default)]
pub struct DeadlineSnapshot {
    deadline: Option<Instant>,
    cancel: Option<CancelToken>,
    limit_ms: u64,
}

impl DeadlineSnapshot {
    /// Same verdict as [`check`], from captured state (no thread-local read).
    /// Workers call this every N pages/candidates.
    pub fn check(&self) -> Result<()> {
        if let Some(c) = &self.cancel {
            if c.is_cancelled() {
                return Err(DbError::QueryCancelled);
            }
        }
        if let Some(d) = self.deadline {
            if Instant::now() >= d {
                return Err(DbError::QueryTimeout {
                    limit_ms: self.limit_ms,
                });
            }
        }
        Ok(())
    }
}

/// Snapshot the current thread's deadline + cancel token for propagation to
/// worker threads. Call it on the query thread *before* spawning workers.
pub fn snapshot_deadline() -> DeadlineSnapshot {
    CURRENT.with(|c| match c.borrow().as_ref() {
        Some(l) => DeadlineSnapshot {
            deadline: l.deadline,
            cancel: l.cancel.clone(),
            limit_ms: l.timeout.map(|d| d.as_millis() as u64).unwrap_or(0),
        },
        None => DeadlineSnapshot::default(),
    })
}

/// The effective `work_mem` row budget: the current query's override if set,
/// else `default` (which the caller derives from its env var / constant).
pub fn work_mem_rows(default: usize) -> usize {
    CURRENT.with(|c| {
        c.borrow()
            .as_ref()
            .and_then(|l| l.work_mem_rows)
            .unwrap_or(default)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_limits_is_a_noop() {
        assert!(check().is_ok());
        assert_eq!(work_mem_rows(42), 42);
    }

    #[test]
    fn expired_deadline_trips_check() {
        let _g = install(QueryLimits::with_timeout(Duration::from_millis(0)));
        std::thread::sleep(Duration::from_millis(1));
        assert!(matches!(check(), Err(DbError::QueryTimeout { .. })));
    }

    #[test]
    fn cancel_token_trips_check_and_guard_restores() {
        let token = CancelToken::new();
        {
            let _g = install(QueryLimits::default().set_cancel(token.clone()));
            assert!(check().is_ok());
            token.cancel();
            assert!(matches!(check(), Err(DbError::QueryCancelled)));
        }
        // Guard dropped → limits cleared → no leak onto this (pooled) thread.
        assert!(check().is_ok());
    }

    #[test]
    fn work_mem_override_takes_precedence() {
        let _g = install(QueryLimits::default().set_work_mem_rows(7));
        assert_eq!(work_mem_rows(1_000_000), 7);
    }
}
