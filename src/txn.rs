// Transaction manager (M1, D10–D12).
//
// A user transaction is a sequence of mini-txns (D2's per-statement atomic
// unit) tied together by a shared xid — not one giant WAL bracket. Each
// statement still gets its own mini-txn (BEGIN/mutations/COMMIT, fsynced
// immediately, unchanged from M0); the *user* transaction's commit/abort
// status is tracked separately via WAL_TXN_BEGIN/COMMIT/ABORT records
// (wal.rs). This keeps ARIES steal+no-force (D1) intact: a multi-statement
// transaction's dirty pages may already be durably logged per-statement long
// before the user transaction itself commits.
//
// READ COMMITTED (default) recomputes a fresh Snapshot at the start of every
// statement; REPEATABLE READ/SI computes one at BEGIN and reuses it for the
// whole transaction (D10). Both share the same visibility check
// (mvcc::is_visible) — only snapshot lifetime differs.

use std::collections::{HashMap, HashSet};

use crate::{
    bufferpool::BufferPool,
    error::{DbError, Result},
    format::{Lsn, PageId, Xid},
    heap::Heap,
    lockmgr::LockManager,
    mvcc::Snapshot,
    wal::{Wal, WalRecord},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    ReadCommitted,
    RepeatableRead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnState {
    Active,
    Committed,
    Aborted,
}

/// One in-memory record of an MVCC mutation performed by a transaction, so
/// an explicit ROLLBACK (or a lock-conflict-forced abort, M1.b) can reverse
/// it. This is rebuilt from the WAL's redo payloads if recovery has to undo
/// an incomplete transaction after a crash instead (see recovery.rs) — it
/// does not need to be WAL-logged itself.
#[derive(Debug, Clone, Copy)]
pub enum UndoAction {
    /// A new tuple version this transaction inserted (INSERT, or an
    /// UPDATE's new-version half). Undo via `Heap::undo_insert`.
    Insert { page_id: PageId, slot: u16 },
    /// An existing tuple whose xmax this transaction stamped (DELETE, or an
    /// UPDATE's old-version half). Undo via `Heap::undo_xmax_stamp`.
    XmaxStamp { page_id: PageId, slot: u16 },
}

pub struct Transaction {
    pub xid: Xid,
    pub isolation: IsolationLevel,
    pub state: TxnState,
    pub snapshot: Snapshot,
    pub begin_lsn: Lsn,
    pub last_lsn: Lsn,
    pub undo_log: Vec<UndoAction>,
}

pub struct TransactionManager {
    next_xid: Xid,
    active: HashMap<Xid, Transaction>,
    committed: HashSet<Xid>,
    aborted: HashSet<Xid>,
}

impl Default for TransactionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TransactionManager {
    pub fn new() -> Self {
        Self::with_next_xid(1)
    }

    pub fn with_next_xid(next_xid: Xid) -> Self {
        Self {
            next_xid,
            active: HashMap::new(),
            committed: HashSet::new(),
            aborted: HashSet::new(),
        }
    }

    /// Determine the xid counter to resume from after a crash: one past the
    /// highest xid that ever began, per the WAL's `WAL_TXN_BEGIN` records.
    pub fn recover_next_xid(records: &[WalRecord]) -> Xid {
        use crate::format::WAL_TXN_BEGIN;
        records
            .iter()
            .filter(|r| r.rec_type == WAL_TXN_BEGIN)
            .map(|r| r.mini_txn_id)
            .max()
            .map(|m| m + 1)
            .unwrap_or(1)
    }

    fn compute_snapshot(&self) -> Snapshot {
        let active_xids: Vec<Xid> = self.active.keys().copied().collect();
        let xmin = active_xids.iter().copied().min().unwrap_or(self.next_xid);
        Snapshot::new(xmin, self.next_xid, active_xids)
    }

    pub fn begin(&mut self, isolation: IsolationLevel, wal: &mut Wal) -> Result<Xid> {
        let xid = self.next_xid;
        self.next_xid += 1;
        let begin_lsn = wal.begin_user_txn(xid)?;
        let snapshot = self.compute_snapshot();
        self.active.insert(
            xid,
            Transaction {
                xid,
                isolation,
                state: TxnState::Active,
                snapshot,
                begin_lsn,
                last_lsn: begin_lsn,
                undo_log: Vec::new(),
            },
        );
        tracing::info!(xid, ?isolation, "transaction begin");
        Ok(xid)
    }

    /// The snapshot a statement inside `xid` should read under: fresh for
    /// READ COMMITTED, the fixed BEGIN-time snapshot for REPEATABLE READ/SI.
    pub fn snapshot_for_statement(&mut self, xid: Xid) -> Result<Snapshot> {
        let isolation = self
            .active
            .get(&xid)
            .ok_or(DbError::TxnNotActive { xid })?
            .isolation;
        if isolation == IsolationLevel::ReadCommitted {
            let fresh = self.compute_snapshot();
            if let Some(txn) = self.active.get_mut(&xid) {
                txn.snapshot = fresh.clone();
            }
            Ok(fresh)
        } else {
            Ok(self.active[&xid].snapshot.clone())
        }
    }

    /// Record a mutation for possible later rollback. Called by the Engine
    /// layer after each successful `Heap` insert/update/delete.
    pub fn record_undo(&mut self, xid: Xid, action: UndoAction) -> Result<()> {
        self.active
            .get_mut(&xid)
            .ok_or(DbError::TxnNotActive { xid })?
            .undo_log
            .push(action);
        Ok(())
    }

    /// Commit `xid`. Note on conflict detection (M1.b, D12): there is no
    /// separate "recheck at commit time" step. Because `LockManager` holds
    /// a row's write lock for the *entire* lifetime of the transaction that
    /// acquired it (released only here or in `abort`), no other transaction
    /// can successfully write to a row this transaction touched between its
    /// write and this commit — the conflict, if any, was already caught
    /// immediately at `Heap::update`/`delete` time via `try_acquire_write`.
    /// This is stronger than needing a distinct commit-time check.
    pub fn commit(&mut self, xid: Xid, wal: &mut Wal, lock_mgr: &mut LockManager) -> Result<()> {
        let txn = self
            .active
            .remove(&xid)
            .ok_or(DbError::TxnNotActive { xid })?;
        wal.commit_user_txn(xid, txn.last_lsn)?;
        self.committed.insert(xid);
        lock_mgr.release_all(xid);
        tracing::info!(xid, "transaction commit");
        Ok(())
    }

    /// Roll back `xid`: physically reverse its writes in reverse order
    /// (self-stamp its own inserts, revert its xmax stamps), then record
    /// the abort. Physical reversal is required for correctness, not just
    /// cleanliness — `mvcc::is_visible` only distinguishes "committed" from
    /// "still active," so a merely-flagged-aborted xid whose tuples were
    /// left untouched would look committed to any snapshot taken after the
    /// abort. See MEMORY.md's design note for the full reasoning.
    pub fn abort(
        &mut self,
        xid: Xid,
        pool: &mut BufferPool,
        heap: &mut Heap,
        wal: &mut Wal,
        lock_mgr: &mut LockManager,
    ) -> Result<()> {
        let txn = self
            .active
            .remove(&xid)
            .ok_or(DbError::TxnNotActive { xid })?;
        for action in txn.undo_log.iter().rev() {
            match *action {
                UndoAction::Insert { page_id, slot } => {
                    heap.undo_insert(page_id, slot, xid, pool, wal)?;
                }
                UndoAction::XmaxStamp { page_id, slot } => {
                    heap.undo_xmax_stamp(page_id, slot, pool, wal)?;
                }
            }
        }
        wal.abort_user_txn(xid, txn.last_lsn)?;
        self.aborted.insert(xid);
        lock_mgr.release_all(xid);
        tracing::info!(xid, "transaction abort");
        Ok(())
    }

    pub fn is_active(&self, xid: Xid) -> bool {
        self.active.contains_key(&xid)
    }

    pub fn is_committed(&self, xid: Xid) -> bool {
        self.committed.contains(&xid)
    }

    pub fn is_aborted(&self, xid: Xid) -> bool {
        self.aborted.contains(&xid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{DEFAULT_PAGE_SIZE, INVALID_LSN};
    use tempfile::tempdir;

    fn setup(dir: &std::path::Path) -> (BufferPool, Heap, Wal) {
        let pool = BufferPool::open(&dir.join("data.db"), DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        let heap = Heap::new(DEFAULT_PAGE_SIZE as usize);
        let wal = Wal::open(&dir.join("db.wal"), INVALID_LSN).unwrap();
        (pool, heap, wal)
    }

    #[test]
    fn begin_assigns_increasing_xids() {
        let dir = tempdir().unwrap();
        let (_pool, _heap, mut wal) = setup(dir.path());
        let mut mgr = TransactionManager::new();
        let a = mgr.begin(IsolationLevel::ReadCommitted, &mut wal).unwrap();
        let b = mgr.begin(IsolationLevel::ReadCommitted, &mut wal).unwrap();
        assert!(b > a);
    }

    #[test]
    fn read_committed_recomputes_snapshot_each_statement() {
        let dir = tempdir().unwrap();
        let (_pool, _heap, mut wal) = setup(dir.path());
        let mut mgr = TransactionManager::new();
        let a = mgr.begin(IsolationLevel::ReadCommitted, &mut wal).unwrap();
        let snap1 = mgr.snapshot_for_statement(a).unwrap();
        let b = mgr.begin(IsolationLevel::ReadCommitted, &mut wal).unwrap();
        let snap2 = mgr.snapshot_for_statement(a).unwrap();
        // b's begin bumped next_xid, so a's second statement sees a wider xmax.
        assert!(snap2.xmax > snap1.xmax);
        let _ = b;
    }

    #[test]
    fn repeatable_read_keeps_fixed_snapshot() {
        let dir = tempdir().unwrap();
        let (_pool, _heap, mut wal) = setup(dir.path());
        let mut mgr = TransactionManager::new();
        let a = mgr.begin(IsolationLevel::RepeatableRead, &mut wal).unwrap();
        let snap1 = mgr.snapshot_for_statement(a).unwrap();
        mgr.begin(IsolationLevel::ReadCommitted, &mut wal).unwrap();
        let snap2 = mgr.snapshot_for_statement(a).unwrap();
        assert_eq!(snap1.xmax, snap2.xmax);
    }

    #[test]
    fn commit_marks_committed_and_removes_from_active() {
        let dir = tempdir().unwrap();
        let (_pool, _heap, mut wal) = setup(dir.path());
        let mut mgr = TransactionManager::new();
        let mut lock_mgr = LockManager::new();
        let a = mgr.begin(IsolationLevel::ReadCommitted, &mut wal).unwrap();
        mgr.commit(a, &mut wal, &mut lock_mgr).unwrap();
        assert!(!mgr.is_active(a));
        assert!(mgr.is_committed(a));
    }

    #[test]
    fn abort_undoes_insert_and_marks_aborted() {
        let dir = tempdir().unwrap();
        let (mut pool, mut heap, mut wal) = setup(dir.path());
        let mut mgr = TransactionManager::new();
        let mut lock_mgr = LockManager::new();
        let a = mgr.begin(IsolationLevel::ReadCommitted, &mut wal).unwrap();
        let rid = heap.insert(b"oops", a, &mut pool, &mut wal).unwrap();
        mgr.record_undo(
            a,
            UndoAction::Insert {
                page_id: rid.page_id,
                slot: rid.slot,
            },
        )
        .unwrap();
        mgr.abort(a, &mut pool, &mut heap, &mut wal, &mut lock_mgr)
            .unwrap();
        assert!(!mgr.is_active(a));
        assert!(mgr.is_aborted(a));
        // A fresh snapshot after the abort must never see the row.
        let snap_after = Snapshot::new(a + 1, a + 1, vec![]);
        assert!(matches!(
            heap.get(rid, &snap_after, a + 1, &mut pool),
            Err(DbError::NoVisibleVersion { .. })
        ));
    }

    #[test]
    fn double_commit_is_an_error() {
        let dir = tempdir().unwrap();
        let (_pool, _heap, mut wal) = setup(dir.path());
        let mut mgr = TransactionManager::new();
        let mut lock_mgr = LockManager::new();
        let a = mgr.begin(IsolationLevel::ReadCommitted, &mut wal).unwrap();
        mgr.commit(a, &mut wal, &mut lock_mgr).unwrap();
        assert!(matches!(
            mgr.commit(a, &mut wal, &mut lock_mgr),
            Err(DbError::TxnNotActive { .. })
        ));
    }

    #[test]
    fn recover_next_xid_resumes_past_highest_seen() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("test.wal");
        let mut wal = Wal::open(&p, INVALID_LSN).unwrap();
        let lsn = wal.begin_user_txn(5).unwrap();
        wal.begin_user_txn(2).unwrap();
        wal.commit_user_txn(5, lsn).unwrap(); // fsync so scan_file sees the records
        let records = Wal::scan_file(&p).unwrap();
        assert_eq!(TransactionManager::recover_next_xid(&records), 6);
    }

    #[test]
    fn recover_next_xid_defaults_to_one_with_no_txns() {
        assert_eq!(TransactionManager::recover_next_xid(&[]), 1);
    }
}
