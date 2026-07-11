//! Server-side SQL result cursors (REST enrichment R4): large-result
//! pagination for `POST /sql`.
//!
//! With `"cursor": true` on `POST /sql`, the query's `rows` result is kept
//! server-side (as decoded [`Literal`] rows, **not** as one giant serialized
//! JSON array) and handed back a page at a time via
//! `GET /sql/cursor/{id}?limit=N`, so a large scan never materializes a
//! multi-hundred-MB JSON response body in memory or on the wire.
//!
//! **Honest cost model:** the engine's executor is synchronous and returns a
//! fully-materialized `Vec` of rows, so the *decoded row data* is buffered
//! server-side for the cursor's lifetime — what a cursor avoids is the
//! (typically several-times-larger) single JSON serialization and its
//! transfer buffering, and it bounds every individual response. True
//! incremental executor streaming would be an engine change, out of scope
//! here (the engine stays sync, CLAUDE.md §4).
//!
//! Cursors are principal-bound like transaction sessions and expire on idle
//! (same background reaper); an expired or exhausted cursor id returns
//! `404 CURSOR_NOT_FOUND`.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

use crate::sql::logical::Literal;

/// Why a cursor fetch failed — mapped to HTTP by `server::error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorError {
    /// Unknown, expired, or already-exhausted cursor → `404 CURSOR_NOT_FOUND`.
    NotFound(u64),
    /// The cursor belongs to a different JWT principal → `403`.
    Forbidden(u64),
}

struct Cursor {
    principal: Option<String>,
    columns: Vec<String>,
    rows: Vec<Vec<Literal>>,
    /// Next unread row index — pages are consumed strictly forward.
    offset: usize,
    last_used: Instant,
}

/// One fetched page.
pub struct CursorPage {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Literal>>,
    /// `true` when the cursor is exhausted (and has been dropped).
    pub done: bool,
    pub remaining: usize,
}

pub struct CursorStore {
    inner: Mutex<HashMap<u64, Cursor>>,
    idle_timeout: Duration,
    next_id: AtomicU64,
}

impl CursorStore {
    pub fn new(idle_timeout: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            idle_timeout,
            next_id: AtomicU64::new(1),
        }
    }

    pub fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Cursor>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Buffer one query result as a new cursor; returns its id.
    pub fn create(
        &self,
        principal: Option<String>,
        columns: Vec<String>,
        rows: Vec<Vec<Literal>>,
    ) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.lock().insert(
            id,
            Cursor {
                principal,
                columns,
                rows,
                offset: 0,
                last_used: Instant::now(),
            },
        );
        id
    }

    /// Fetch the next `limit` rows. An exhausted cursor is removed and its
    /// final page reports `done: true`; a later fetch gets `NotFound` —
    /// matching the session registry's ephemerality contract.
    pub fn fetch(
        &self,
        id: u64,
        principal: &Option<String>,
        limit: usize,
    ) -> Result<CursorPage, CursorError> {
        let mut map = self.lock();
        let cursor = map.get_mut(&id).ok_or(CursorError::NotFound(id))?;
        if cursor.principal != *principal {
            return Err(CursorError::Forbidden(id));
        }
        let end = cursor.rows.len().min(cursor.offset + limit.max(1));
        let rows = cursor.rows[cursor.offset..end].to_vec();
        cursor.offset = end;
        cursor.last_used = Instant::now();
        let remaining = cursor.rows.len() - cursor.offset;
        let done = remaining == 0;
        let columns = cursor.columns.clone();
        if done {
            map.remove(&id);
        }
        Ok(CursorPage {
            columns,
            rows,
            done,
            remaining,
        })
    }

    /// Drop a cursor early (client is done with it).
    pub fn remove(&self, id: u64, principal: &Option<String>) -> Result<(), CursorError> {
        let mut map = self.lock();
        let cursor = map.get(&id).ok_or(CursorError::NotFound(id))?;
        if cursor.principal != *principal {
            return Err(CursorError::Forbidden(id));
        }
        map.remove(&id);
        Ok(())
    }

    /// Drop every idle-expired cursor (called by the background reaper).
    /// Returns how many were reclaimed.
    pub fn sweep(&self) -> usize {
        let mut map = self.lock();
        let before = map.len();
        map.retain(|_, c| c.last_used.elapsed() < self.idle_timeout);
        before - map.len()
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(n: usize) -> Vec<Vec<Literal>> {
        (0..n).map(|i| vec![Literal::Int(i as i64)]).collect()
    }

    #[test]
    fn pages_forward_and_expires_when_exhausted() {
        let store = CursorStore::new(Duration::from_secs(60));
        let id = store.create(None, vec!["id".into()], rows(5));

        let p1 = store.fetch(id, &None, 2).unwrap();
        assert_eq!(p1.rows.len(), 2);
        assert!(!p1.done);
        assert_eq!(p1.remaining, 3);

        let p2 = store.fetch(id, &None, 10).unwrap();
        assert_eq!(p2.rows.len(), 3);
        assert!(p2.done);
        assert_eq!(p2.remaining, 0);

        // Exhausted → removed → subsequent fetch is NotFound.
        assert_eq!(
            store.fetch(id, &None, 1).err(),
            Some(CursorError::NotFound(id))
        );
    }

    #[test]
    fn cursor_is_principal_bound() {
        let store = CursorStore::new(Duration::from_secs(60));
        let id = store.create(Some("alice".into()), vec!["id".into()], rows(1));
        assert_eq!(
            store.fetch(id, &Some("bob".into()), 1).err(),
            Some(CursorError::Forbidden(id))
        );
        assert_eq!(
            store.remove(id, &None).err(),
            Some(CursorError::Forbidden(id))
        );
        assert!(store.fetch(id, &Some("alice".into()), 1).is_ok());
    }

    #[test]
    fn sweep_reclaims_idle_cursors() {
        let store = CursorStore::new(Duration::from_millis(10));
        store.create(None, vec!["id".into()], rows(1));
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(store.sweep(), 1);
        assert!(store.is_empty());
    }
}
