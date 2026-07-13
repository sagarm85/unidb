//! Correlation-id plumbing (item 22, L2).
//!
//! The server assigns a `request_id` to every HTTP request; the SQL path tags
//! the transaction id (`xid`). To make one request's lines joinable across the
//! three log surfaces — the structured app log (`tracing`), the slow-query log
//! (a `tracing::warn` inside the executor), and the security `audit.log` file —
//! those ids have to reach code deep in the (synchronous) engine core.
//!
//! `txn_id` is threaded directly (every relevant call already has the `xid`).
//! `request_id`, however, is a *server* concept the engine core knows nothing
//! about, and engine calls run on `spawn_blocking` pool threads where neither an
//! async task-local nor an entered `tracing` span propagates. So the server sets
//! this **thread-local** at the top of each blocking engine call (via the RAII
//! [`RequestIdGuard`]), and engine-core logging reads it back with
//! [`current_request_id`].
//!
//! This lives in the default (non-`server`) build deliberately: it is plain
//! `std` with no new dependency, so the "engine stays sync, `tracing` only" rule
//! holds. In an embedded (non-server) process the thread-local is simply never
//! set and [`current_request_id`] returns `None`.

use std::cell::RefCell;

thread_local! {
    static REQUEST_ID: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Restores the previous `request_id` when dropped, so nested/reused pool
/// threads never leak a stale id into an unrelated later call.
#[must_use = "the request id is cleared when this guard is dropped"]
pub struct RequestIdGuard(Option<String>);

impl Drop for RequestIdGuard {
    fn drop(&mut self) {
        let prev = self.0.take();
        REQUEST_ID.with(|c| *c.borrow_mut() = prev);
    }
}

/// Set the current thread's `request_id` for the lifetime of the returned
/// guard. Passing `None` explicitly clears it (e.g. an internal call with no
/// originating request). Idempotent and cheap — one `RefCell` swap.
pub fn set_request_id(id: Option<String>) -> RequestIdGuard {
    let prev = REQUEST_ID.with(|c| c.replace(id));
    RequestIdGuard(prev)
}

/// The current thread's `request_id`, if one was set by the server bridge.
pub fn current_request_id() -> Option<String> {
    REQUEST_ID.with(|c| c.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_by_default() {
        assert_eq!(current_request_id(), None);
    }

    #[test]
    fn guard_sets_and_restores() {
        assert_eq!(current_request_id(), None);
        {
            let _g = set_request_id(Some("req-abc".into()));
            assert_eq!(current_request_id().as_deref(), Some("req-abc"));
            {
                // Nested scope shadows, then restores the outer value on drop.
                let _g2 = set_request_id(Some("req-def".into()));
                assert_eq!(current_request_id().as_deref(), Some("req-def"));
            }
            assert_eq!(current_request_id().as_deref(), Some("req-abc"));
        }
        assert_eq!(current_request_id(), None);
    }
}
