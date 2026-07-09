// Lock manager (M1.b, D12).
//
// Keyed by (record_kind, record_id) per the architecture doc's stated design
// — only `RecordKind::Row` exists in M1; `Vector`/`GraphEdge`/`QueueEvent`
// variants land in M2+ without reshaping this key.
//
// Write-write conflicts only: under MVCC, readers never block writers or
// other readers (that's the whole point of MVCC), so there is no read-lock
// concept here. Per D12, SI's conflict handling is "abort," not
// "block-and-wait" — `try_acquire_write` either succeeds immediately or the
// caller aborts. No wait queue, no deadlock detection: that complexity is
// deliberately deferred to a future SERIALIZABLE/SSI effort, which is
// exactly what the `concurrency_hooks` seam (D11) exists to bolt on without
// reworking this module.
//
// Locks are in-memory only, not WAL-logged — they are a concurrency-control
// mechanism for the currently-open transaction table, not a durability
// concern. Any in-flight transaction is implicitly aborted by recovery on
// restart (the transaction table itself doesn't survive a crash), so there
// is nothing to recover here.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use crate::{
    error::{DbError, Result},
    format::{PageId, Xid},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordKind {
    Row,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecordId {
    pub kind: RecordKind,
    pub id: u64,
}

impl RecordId {
    /// Pack a heap RowId's (page_id, slot) into a single u64 key, generic
    /// enough that other kinds can reuse this u64-keyed shape later.
    pub fn row(page_id: PageId, slot: u16) -> Self {
        Self {
            kind: RecordKind::Row,
            id: ((page_id as u64) << 16) | slot as u64,
        }
    }
}

/// Write-lock table (M1.b). **P5.c** moved its state behind a `Mutex` and made
/// every method `&self`, so an `Arc<LockManager>` can be shared across the
/// concurrent writer threads P5.e introduces (an owned `&mut LockManager` on
/// the single-writer path still works unchanged — `&mut` derefs to `&`). The
/// policy is unchanged: abort-on-conflict, no waiting. **P5.d** will add lock
/// modes, real blocking wait queues, and wait-for-graph deadlock detection on
/// top of this same `&self` surface.
#[derive(Default)]
pub struct LockManager {
    write_locks: Mutex<HashMap<RecordId, Xid>>,
}

impl LockManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Poison-safe access to the lock table: a prior panic-while-locked leaves
    /// the map usable as-is (consistent with `txn.rs`/`wal.rs`), so a single
    /// poisoned mutation never cascades into a crash on every later lock op.
    fn lock(&self) -> MutexGuard<'_, HashMap<RecordId, Xid>> {
        self.write_locks.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Attempt to acquire (or re-acquire, if `xid` already holds it) a write
    /// intent on `id`. Fails immediately with `WriteConflict` if another
    /// *active* xid already holds it — no waiting.
    pub fn try_acquire_write(&self, id: RecordId, xid: Xid) -> Result<()> {
        let mut locks = self.lock();
        match locks.get(&id) {
            Some(&holder) if holder != xid => Err(DbError::WriteConflict { holder_xid: holder }),
            _ => {
                locks.insert(id, xid);
                Ok(())
            }
        }
    }

    /// Release every write lock held by `xid` (called on commit or abort).
    pub fn release_all(&self, xid: Xid) {
        self.lock().retain(|_, holder| *holder != xid);
    }

    pub fn holder(&self, id: RecordId) -> Option<Xid> {
        self.lock().get(&id).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_succeeds_when_free() {
        let lm = LockManager::new();
        let r = RecordId::row(1, 0);
        assert!(lm.try_acquire_write(r, 1).is_ok());
        assert_eq!(lm.holder(r), Some(1));
    }

    #[test]
    fn second_acquire_by_different_xid_conflicts() {
        let lm = LockManager::new();
        let r = RecordId::row(1, 0);
        lm.try_acquire_write(r, 1).unwrap();
        let err = lm.try_acquire_write(r, 2);
        assert!(matches!(err, Err(DbError::WriteConflict { holder_xid: 1 })));
    }

    #[test]
    fn same_xid_reacquiring_is_idempotent() {
        let lm = LockManager::new();
        let r = RecordId::row(1, 0);
        lm.try_acquire_write(r, 1).unwrap();
        assert!(lm.try_acquire_write(r, 1).is_ok());
    }

    #[test]
    fn release_all_frees_locks_for_others() {
        let lm = LockManager::new();
        let r = RecordId::row(1, 0);
        lm.try_acquire_write(r, 1).unwrap();
        lm.release_all(1);
        assert_eq!(lm.holder(r), None);
        assert!(lm.try_acquire_write(r, 2).is_ok());
    }

    #[test]
    fn release_all_only_affects_the_given_xid() {
        let lm = LockManager::new();
        let r1 = RecordId::row(1, 0);
        let r2 = RecordId::row(1, 1);
        lm.try_acquire_write(r1, 1).unwrap();
        lm.try_acquire_write(r2, 2).unwrap();
        lm.release_all(1);
        assert_eq!(lm.holder(r1), None);
        assert_eq!(lm.holder(r2), Some(2));
    }

    #[test]
    fn different_kinds_or_ids_do_not_collide() {
        let lm = LockManager::new();
        let a = RecordId::row(1, 0);
        let b = RecordId::row(1, 1);
        let c = RecordId::row(2, 0);
        assert!(lm.try_acquire_write(a, 1).is_ok());
        assert!(lm.try_acquire_write(b, 2).is_ok());
        assert!(lm.try_acquire_write(c, 3).is_ok());
    }
}
