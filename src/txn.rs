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
use std::sync::{Arc, Mutex, MutexGuard};

use crate::{
    bufferpool::BufferPool,
    error::{DbError, Result},
    format::{Lsn, PageId, Xid},
    heap::{Heap, RowId},
    lockmgr::LockManager,
    mvcc::Snapshot,
    wal::{Wal, WalRecord},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    ReadCommitted,
    RepeatableRead,
    /// True serializability via SSI (P1.d). Uses the same fixed BEGIN-time
    /// snapshot as `RepeatableRead` (so it never sees anomalies RR would),
    /// **plus** rw-antidependency tracking: a transaction that forms a
    /// dangerous structure (an inbound *and* an outbound rw-conflict — a
    /// pivot) is aborted with [`DbError::SerializationFailure`] rather than
    /// committing a non-serializable schedule (e.g. write-skew). See the SSI
    /// tracker below.
    Serializable,
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
    /// SSI rw-antidependency tracking (P1.d), populated only for
    /// `Serializable` transactions. `None` for RC/RR (no overhead).
    pub ssi: Option<SsiState>,
}

/// Per-transaction SSI state (P1.d, Cahill-style rw-antidependency tracking).
/// A transaction records the rows it read and wrote, and two flags: an
/// **incoming** rw-conflict (a concurrent transaction read a row this one then
/// wrote — someone rw-depends *on* us) and an **outgoing** rw-conflict (this
/// transaction read a row a concurrent one then wrote — we rw-depend on them).
/// A transaction that ends up with *both* is a **pivot** in a dangerous
/// structure and is aborted at commit rather than committing a non-serializable
/// schedule. Row-granularity (no predicate locks), so this catches write-skew
/// on existing rows but not phantom anomalies — the reduced form the plan
/// allows.
#[derive(Debug, Default, Clone)]
pub struct SsiState {
    pub reads: HashSet<RowId>,
    pub writes: HashSet<RowId>,
    pub in_conflict: bool,
    pub out_conflict: bool,
}

impl SsiState {
    /// A pivot: has both an inbound and an outbound rw-antidependency.
    fn is_pivot(&self) -> bool {
        self.in_conflict && self.out_conflict
    }
}

/// The snapshot-relevant transaction state, shared between the writer's
/// `TransactionManager` and concurrent readers' `ReadHandle` (6b) behind an
/// `Arc<Mutex<..>>`. The writer is the only mutator; readers lock briefly to
/// build an MVCC snapshot for a statement. `undo_log` lives here too but is
/// only ever touched on the (single-threaded) write path.
pub struct TxnInner {
    next_xid: Xid,
    active: HashMap<Xid, Transaction>,
    committed: HashSet<Xid>,
    aborted: HashSet<Xid>,
    /// Live concurrent-reader snapshots (M10.a). A `ReadHandle` read allocates
    /// no xid and never enters `active`, so without this a long-running
    /// off-writer-thread reader would be invisible to `vacuum_horizon` — and
    /// the writer could reclaim a tuple version that reader's in-flight scan
    /// still needs. Each entry is a live read snapshot's `xmin`, keyed by a
    /// registration id so it can be dropped when the read finishes (see
    /// [`ReadRegistration`]). Held only for the duration of one read call.
    read_registrations: HashMap<u64, Xid>,
    next_reg_id: u64,
    /// SSI state of `Serializable` transactions that have **committed** but may
    /// still be concurrent with a live serializable transaction (P1.d), kept so
    /// a later read/write by that live transaction can still form an
    /// rw-antidependency edge with them. Cleared whenever no serializable
    /// transaction is active (nothing left that could conflict). Aborted
    /// transactions are never added — their writes are physically undone.
    committed_ser: HashMap<Xid, SsiState>,
}

impl TxnInner {
    fn compute_snapshot(&self) -> Snapshot {
        let active_xids: Vec<Xid> = self.active.keys().copied().collect();
        let xmin = active_xids.iter().copied().min().unwrap_or(self.next_xid);
        Snapshot::new(xmin, self.next_xid, active_xids)
    }

    /// The vacuum horizon (`OldestXmin`, M10.a): the minimum `snapshot.xmin`
    /// across every live writer transaction **and** every live concurrent
    /// reader (6b). A tuple version whose committed `xmax` is strictly below
    /// this can never again be seen as live by any current or future snapshot,
    /// so it is safe to physically reclaim. Conservative on purpose: a
    /// long-lived `REPEATABLE READ` transaction (or a slow reader) legitimately
    /// holds the horizon back and blocks reclamation — the same behavior, and
    /// the same operational footgun, as Postgres. Falls back to `next_xid`
    /// when nothing is live (everything below it is then reclaimable).
    fn vacuum_horizon(&self) -> Xid {
        let writers = self.active.values().map(|t| t.snapshot.xmin);
        let readers = self.read_registrations.values().copied();
        writers.chain(readers).min().unwrap_or(self.next_xid)
    }

    /// The snapshot a statement inside `xid` should read under: fresh for
    /// READ COMMITTED, the fixed BEGIN-time snapshot for REPEATABLE READ/SI.
    fn snapshot_for_statement(&mut self, xid: Xid) -> Result<Snapshot> {
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
}

/// Shared handle to [`TxnInner`], cloneable and `Send + Sync` — a `ReadHandle`
/// holds one to compute snapshots off the writer thread.
pub type SharedTxn = Arc<Mutex<TxnInner>>;

/// Compute a statement snapshot for `xid` from shared txn state. Used by the
/// concurrent read path (`ReadHandle`); the writer uses
/// [`TransactionManager::snapshot_for_statement`], which delegates here.
pub fn snapshot_for_statement(shared: &SharedTxn, xid: Xid) -> Result<Snapshot> {
    lock_txn(shared).snapshot_for_statement(xid)
}

/// A live-reader registration (M10.a). While one of these is alive, the
/// reader's snapshot `xmin` is included in [`TransactionManager::
/// vacuum_horizon`], so the writer thread cannot reclaim a tuple version this
/// concurrent read still needs. Dropped (deregistered) automatically when the
/// read finishes — the [`Drop`] impl is the whole point, so callers must hold
/// it for the entire duration of the read, not discard it eagerly.
#[must_use = "hold the registration for the whole read; dropping it early lets vacuum reclaim rows the read still needs"]
pub struct ReadRegistration {
    shared: SharedTxn,
    id: u64,
}

impl Drop for ReadRegistration {
    fn drop(&mut self) {
        lock_txn(&self.shared).read_registrations.remove(&self.id);
    }
}

/// A self-contained READ COMMITTED snapshot for a **read-only** statement that
/// never enters the writer thread (6b): no xid is allocated, no `WAL_TXN_BEGIN`
/// is written. Returns the snapshot plus a sentinel `self_xid` (the current
/// `next_xid`, which no committed or active transaction can equal), so
/// `mvcc::is_visible`'s "my own uncommitted write" branch is never taken — a
/// read-only reader has no writes of its own to see — plus a
/// [`ReadRegistration`] that holds the vacuum horizon back for the life of the
/// read (M10.a). The registration must be kept alive until the read's pages
/// have all been consumed.
pub fn read_snapshot(shared: &SharedTxn) -> (Snapshot, Xid, ReadRegistration) {
    let mut inner = lock_txn(shared);
    let snapshot = inner.compute_snapshot();
    let self_xid = inner.next_xid;
    let id = inner.next_reg_id;
    inner.next_reg_id += 1;
    inner.read_registrations.insert(id, snapshot.xmin);
    (
        snapshot,
        self_xid,
        ReadRegistration {
            shared: Arc::clone(shared),
            id,
        },
    )
}

fn lock_txn(shared: &SharedTxn) -> MutexGuard<'_, TxnInner> {
    // Recover from a poisoned lock rather than panicking (a poisoned txn map
    // means a prior panic-while-locked; proceed with the state as-is).
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct TransactionManager {
    inner: SharedTxn,
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
            inner: Arc::new(Mutex::new(TxnInner {
                next_xid,
                active: HashMap::new(),
                committed: HashSet::new(),
                aborted: HashSet::new(),
                read_registrations: HashMap::new(),
                next_reg_id: 1,
                committed_ser: HashMap::new(),
            })),
        }
    }

    /// A cloneable shared handle to the snapshot-relevant txn state, for the
    /// concurrent read path (6b).
    pub fn shared(&self) -> SharedTxn {
        Arc::clone(&self.inner)
    }

    fn lock(&self) -> MutexGuard<'_, TxnInner> {
        lock_txn(&self.inner)
    }

    /// The next xid that will be issued by [`Self::begin`]. Persisted into
    /// the control file at every checkpoint (`checkpoint::run`) so
    /// `Engine::open` can resume correctly even after the WAL has been
    /// truncated and no longer has any `WAL_TXN_BEGIN` record to scan.
    pub fn next_xid(&self) -> Xid {
        self.lock().next_xid
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

    pub fn begin(&self, isolation: IsolationLevel, wal: &Wal) -> Result<Xid> {
        let mut inner = self.lock();
        let xid = inner.next_xid;
        inner.next_xid += 1;
        let begin_lsn = wal.begin_user_txn(xid)?;
        let snapshot = inner.compute_snapshot();
        inner.active.insert(
            xid,
            Transaction {
                xid,
                isolation,
                state: TxnState::Active,
                snapshot,
                begin_lsn,
                last_lsn: begin_lsn,
                undo_log: Vec::new(),
                ssi: if isolation == IsolationLevel::Serializable {
                    Some(SsiState::default())
                } else {
                    None
                },
            },
        );
        tracing::info!(xid, ?isolation, "transaction begin");
        Ok(xid)
    }

    /// The snapshot a statement inside `xid` should read under: fresh for
    /// READ COMMITTED, the fixed BEGIN-time snapshot for REPEATABLE READ/SI.
    pub fn snapshot_for_statement(&self, xid: Xid) -> Result<Snapshot> {
        self.lock().snapshot_for_statement(xid)
    }

    /// The vacuum horizon (`OldestXmin`, M10.a) — see [`TxnInner::
    /// vacuum_horizon`]. Includes both live writer transactions and live
    /// concurrent readers (6b `ReadHandle`s).
    pub fn vacuum_horizon(&self) -> Xid {
        self.lock().vacuum_horizon()
    }

    /// Record a mutation for possible later rollback. Called by the Engine
    /// layer after each successful `Heap` insert/update/delete.
    pub fn record_undo(&self, xid: Xid, action: UndoAction) -> Result<()> {
        self.lock()
            .active
            .get_mut(&xid)
            .ok_or(DbError::TxnNotActive { xid })?
            .undo_log
            .push(action);
        Ok(())
    }

    /// The isolation level `xid` is running under, if it is active. Used by
    /// the executor to classify a write-write conflict (P1.d): a serialization
    /// anomaly under RR/Serializable, a plain no-wait conflict under RC.
    pub fn isolation(&self, xid: Xid) -> Option<IsolationLevel> {
        self.lock().active.get(&xid).map(|t| t.isolation)
    }

    /// SSI (P1.d): record that serializable `xid` read `rows` (from a scan),
    /// and form an outbound rw-antidependency to every concurrent serializable
    /// transaction that *wrote* any of those rows (we read a version they
    /// superseded). No-op for RC/RR transactions.
    pub fn ssi_note_reads(&self, xid: Xid, rows: &[RowId]) {
        let mut inner = self.lock();
        if inner.active.get(&xid).is_none_or(|t| t.ssi.is_none()) {
            return;
        }
        for &r in rows {
            // Writers of r among *other* serializable txns (active or committed).
            let active_writers: Vec<Xid> = inner
                .active
                .iter()
                .filter(|(&o, t)| o != xid && t.ssi.as_ref().is_some_and(|s| s.writes.contains(&r)))
                .map(|(&o, _)| o)
                .collect();
            let committed_writer = inner
                .committed_ser
                .iter()
                .any(|(&o, s)| o != xid && s.writes.contains(&r));
            if let Some(s) = inner.active.get_mut(&xid).and_then(|t| t.ssi.as_mut()) {
                s.reads.insert(r);
                if !active_writers.is_empty() || committed_writer {
                    s.out_conflict = true; // we rw-depend on a concurrent writer
                }
            }
            for w in active_writers {
                if let Some(s) = inner.active.get_mut(&w).and_then(|t| t.ssi.as_mut()) {
                    s.in_conflict = true; // a concurrent reader (us) depends on them
                }
            }
        }
    }

    /// SSI (P1.d): record that serializable `xid` wrote `row`, and form an
    /// inbound rw-antidependency from every concurrent serializable transaction
    /// that *read* that row (they read a version we superseded). No-op for
    /// RC/RR transactions.
    pub fn ssi_note_write(&self, xid: Xid, row: RowId) {
        let mut inner = self.lock();
        if inner.active.get(&xid).is_none_or(|t| t.ssi.is_none()) {
            return;
        }
        let active_readers: Vec<Xid> = inner
            .active
            .iter()
            .filter(|(&o, t)| o != xid && t.ssi.as_ref().is_some_and(|s| s.reads.contains(&row)))
            .map(|(&o, _)| o)
            .collect();
        let committed_reader = inner
            .committed_ser
            .iter()
            .any(|(&o, s)| o != xid && s.reads.contains(&row));
        if let Some(s) = inner.active.get_mut(&xid).and_then(|t| t.ssi.as_mut()) {
            s.writes.insert(row);
            if !active_readers.is_empty() || committed_reader {
                s.in_conflict = true; // a concurrent reader rw-depends on us
            }
        }
        for rdr in active_readers {
            if let Some(s) = inner.active.get_mut(&rdr).and_then(|t| t.ssi.as_mut()) {
                s.out_conflict = true; // they rw-depend on our write
            }
        }
    }

    /// SSI (P1.d): is `xid` a pivot (both an inbound and an outbound
    /// rw-antidependency) — i.e. must it abort rather than commit? Always
    /// `false` for non-serializable transactions.
    pub fn ssi_is_pivot(&self, xid: Xid) -> bool {
        self.lock()
            .active
            .get(&xid)
            .and_then(|t| t.ssi.as_ref())
            .is_some_and(|s| s.is_pivot())
    }

    /// Commit `xid`. Note on conflict detection (M1.b, D12): there is no
    /// separate "recheck at commit time" step. Because `LockManager` holds
    /// a row's write lock for the *entire* lifetime of the transaction that
    /// acquired it (released only here or in `abort`), no other transaction
    /// can successfully write to a row this transaction touched between its
    /// write and this commit — the conflict, if any, was already caught
    /// immediately at `Heap::update`/`delete` time via `try_acquire_write`.
    /// This is stronger than needing a distinct commit-time check.
    pub fn commit(&self, xid: Xid, wal: &Wal, lock_mgr: &LockManager) -> Result<()> {
        // SSI (P1.d): a serializable pivot must not commit — it would seal a
        // non-serializable schedule (e.g. write-skew). Refuse *before* removing
        // it from `active`, leaving it live for the caller to roll back
        // (`Engine::commit` turns this into an abort + `SerializationFailure`).
        if self.ssi_is_pivot(xid) {
            tracing::info!(xid, "SSI: aborting serializable pivot at commit");
            return Err(DbError::SerializationFailure { xid });
        }
        let txn = self
            .lock()
            .active
            .remove(&xid)
            .ok_or(DbError::TxnNotActive { xid })?;
        // Read-only transactions (nothing recorded in `undo_log`) have no
        // changes to make durable, so they write no WAL_TXN_COMMIT record and
        // pay no fsync — the same optimization Postgres/SQLite apply. Safe
        // because recovery classifies the orphan WAL_TXN_BEGIN as an
        // incomplete user txn whose undo pass finds no mutations owned by
        // `xid` to reverse (see recovery.rs), and no committed tuple ever
        // references a read-only xid's xmin/xmax. Fixes the M1.d "read-only
        // commit pays an unnecessary fsync" regression noted in MEMORY.md.
        if !txn.undo_log.is_empty() {
            wal.commit_user_txn(xid, txn.last_lsn)?;
        }
        {
            let mut inner = self.lock();
            inner.committed.insert(xid);
            // SSI (P1.d): keep this serializable txn's read/write sets available
            // to still-concurrent serializable txns for edge detection; drop all
            // committed-ser state once nothing serializable is active.
            if let Some(ssi) = txn.ssi {
                inner.committed_ser.insert(xid, ssi);
            }
            if !inner.active.values().any(|t| t.ssi.is_some()) {
                inner.committed_ser.clear();
            }
        }
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
        &self,
        xid: Xid,
        pool: &BufferPool,
        heap: &Heap,
        wal: &Wal,
        lock_mgr: &LockManager,
    ) -> Result<()> {
        let txn = self
            .lock()
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
        {
            let mut inner = self.lock();
            inner.aborted.insert(xid);
            // SSI (P1.d): an aborted txn's writes are physically undone, so it
            // never enters `committed_ser`; drop committed-ser state once
            // nothing serializable remains active.
            if !inner.active.values().any(|t| t.ssi.is_some()) {
                inner.committed_ser.clear();
            }
        }
        lock_mgr.release_all(xid);
        tracing::info!(xid, "transaction abort");
        Ok(())
    }

    pub fn is_active(&self, xid: Xid) -> bool {
        self.lock().active.contains_key(&xid)
    }

    /// Number of transactions currently open (P1.e). Auto-checkpoint fires only
    /// when this is zero — a quiescent point — so a checkpoint's WAL truncation
    /// can never discard an in-flight transaction's undo records.
    pub fn active_count(&self) -> usize {
        self.lock().active.len()
    }

    pub fn is_committed(&self, xid: Xid) -> bool {
        self.lock().committed.contains(&xid)
    }

    pub fn is_aborted(&self, xid: Xid) -> bool {
        self.lock().aborted.contains(&xid)
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
        let (_pool, _heap, wal) = setup(dir.path());
        let mgr = TransactionManager::new();
        let a = mgr.begin(IsolationLevel::ReadCommitted, &wal).unwrap();
        let b = mgr.begin(IsolationLevel::ReadCommitted, &wal).unwrap();
        assert!(b > a);
    }

    #[test]
    fn read_committed_recomputes_snapshot_each_statement() {
        let dir = tempdir().unwrap();
        let (_pool, _heap, wal) = setup(dir.path());
        let mgr = TransactionManager::new();
        let a = mgr.begin(IsolationLevel::ReadCommitted, &wal).unwrap();
        let snap1 = mgr.snapshot_for_statement(a).unwrap();
        let b = mgr.begin(IsolationLevel::ReadCommitted, &wal).unwrap();
        let snap2 = mgr.snapshot_for_statement(a).unwrap();
        // b's begin bumped next_xid, so a's second statement sees a wider xmax.
        assert!(snap2.xmax > snap1.xmax);
        let _ = b;
    }

    #[test]
    fn repeatable_read_keeps_fixed_snapshot() {
        let dir = tempdir().unwrap();
        let (_pool, _heap, wal) = setup(dir.path());
        let mgr = TransactionManager::new();
        let a = mgr.begin(IsolationLevel::RepeatableRead, &wal).unwrap();
        let snap1 = mgr.snapshot_for_statement(a).unwrap();
        mgr.begin(IsolationLevel::ReadCommitted, &wal).unwrap();
        let snap2 = mgr.snapshot_for_statement(a).unwrap();
        assert_eq!(snap1.xmax, snap2.xmax);
    }

    #[test]
    fn commit_marks_committed_and_removes_from_active() {
        let dir = tempdir().unwrap();
        let (_pool, _heap, wal) = setup(dir.path());
        let mgr = TransactionManager::new();
        let lock_mgr = LockManager::new();
        let a = mgr.begin(IsolationLevel::ReadCommitted, &wal).unwrap();
        mgr.commit(a, &wal, &lock_mgr).unwrap();
        assert!(!mgr.is_active(a));
        assert!(mgr.is_committed(a));
    }

    #[test]
    fn abort_undoes_insert_and_marks_aborted() {
        let dir = tempdir().unwrap();
        let (mut pool, mut heap, mut wal) = setup(dir.path());
        let mgr = TransactionManager::new();
        let lock_mgr = LockManager::new();
        let a = mgr.begin(IsolationLevel::ReadCommitted, &wal).unwrap();
        let rid = heap.insert(b"oops", a, &pool, &wal).unwrap();
        mgr.record_undo(
            a,
            UndoAction::Insert {
                page_id: rid.page_id,
                slot: rid.slot,
            },
        )
        .unwrap();
        mgr.abort(a, &pool, &heap, &wal, &lock_mgr)
            .unwrap();
        assert!(!mgr.is_active(a));
        assert!(mgr.is_aborted(a));
        // A fresh snapshot after the abort must never see the row.
        let snap_after = Snapshot::new(a + 1, a + 1, vec![]);
        assert!(matches!(
            heap.get(rid, &snap_after, a + 1, &pool),
            Err(DbError::NoVisibleVersion { .. })
        ));
    }

    #[test]
    fn double_commit_is_an_error() {
        let dir = tempdir().unwrap();
        let (_pool, _heap, wal) = setup(dir.path());
        let mgr = TransactionManager::new();
        let lock_mgr = LockManager::new();
        let a = mgr.begin(IsolationLevel::ReadCommitted, &wal).unwrap();
        mgr.commit(a, &wal, &lock_mgr).unwrap();
        assert!(matches!(
            mgr.commit(a, &wal, &lock_mgr),
            Err(DbError::TxnNotActive { .. })
        ));
    }

    #[test]
    fn recover_next_xid_resumes_past_highest_seen() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("test.wal");
        let wal = Wal::open(&p, INVALID_LSN).unwrap();
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

    // ── M10.a: vacuum horizon ────────────────────────────────────────────────

    #[test]
    fn horizon_is_next_xid_when_nothing_live() {
        let mgr = TransactionManager::with_next_xid(42);
        assert_eq!(mgr.vacuum_horizon(), 42);
    }

    #[test]
    fn long_lived_rr_txn_pins_the_horizon() {
        let dir = tempdir().unwrap();
        let (_pool, _heap, wal) = setup(dir.path());
        let mgr = TransactionManager::new();
        let lock_mgr = LockManager::new();

        // A long-lived RR reader takes its snapshot early and never finishes.
        let rr = mgr.begin(IsolationLevel::RepeatableRead, &wal).unwrap();
        // Later transactions come and go.
        for _ in 0..5 {
            let x = mgr.begin(IsolationLevel::ReadCommitted, &wal).unwrap();
            mgr.commit(x, &wal, &lock_mgr).unwrap();
        }
        // The horizon is still pinned to rr's fixed snapshot xmin — a version
        // a later transaction deleted is NOT yet reclaimable while rr lives.
        let pinned = mgr.snapshot_for_statement(rr).unwrap().xmin;
        assert_eq!(mgr.vacuum_horizon(), pinned);

        mgr.commit(rr, &wal, &lock_mgr).unwrap();
        // Once rr finishes, the horizon advances past where rr held it.
        assert!(mgr.vacuum_horizon() > pinned);
    }

    #[test]
    fn concurrent_reader_registration_holds_horizon_back() {
        let dir = tempdir().unwrap();
        let (_pool, _heap, wal) = setup(dir.path());
        let mgr = TransactionManager::new();
        let lock_mgr = LockManager::new();

        // Advance next_xid past 1 by running a couple of transactions.
        for _ in 0..3 {
            let x = mgr.begin(IsolationLevel::ReadCommitted, &wal).unwrap();
            mgr.commit(x, &wal, &lock_mgr).unwrap();
        }
        let shared = mgr.shared();
        // With nothing live, the horizon is next_xid.
        let free_horizon = mgr.vacuum_horizon();

        // A concurrent reader takes a snapshot: its registration holds the
        // horizon at that snapshot's xmin for as long as the guard lives.
        let (snap, _self_xid, reg) = super::read_snapshot(&shared);
        assert_eq!(mgr.vacuum_horizon(), snap.xmin);
        assert!(mgr.vacuum_horizon() <= free_horizon);

        // Dropping the registration releases the hold.
        drop(reg);
        assert_eq!(mgr.vacuum_horizon(), free_horizon);
    }

    // ── P5.c: concurrency stress / linearizability ───────────────────────────
    //
    // Exercise the transaction manager, WAL, and lock manager through the
    // `&self` surfaces P5.a/P5.b/P5.c established, under many real OS threads.
    // These share `&mgr`/`&wal`/`&lock_mgr` via scoped threads (all `Sync`
    // now), so a data race is a compile error; an accounting or lock-ordering
    // bug surfaces as a wrong count, a violated invariant, or a hang.

    #[test]
    fn concurrent_begin_commit_allocate_unique_monotonic_xids() {
        use std::collections::HashSet;
        let dir = tempdir().unwrap();
        let (_pool, _heap, wal) = setup(dir.path());
        let mgr = TransactionManager::new();
        let lock_mgr = LockManager::new();

        const THREADS: usize = 8;
        const PER_THREAD: usize = 100;
        let all: Vec<Xid> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    s.spawn(|| {
                        let mut mine = Vec::with_capacity(PER_THREAD);
                        for _ in 0..PER_THREAD {
                            let x = mgr.begin(IsolationLevel::ReadCommitted, &wal).unwrap();
                            mine.push(x);
                            mgr.commit(x, &wal, &lock_mgr).unwrap();
                        }
                        mine
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect()
        });

        // Every begin under contention handed out a distinct xid...
        assert_eq!(all.len(), THREADS * PER_THREAD);
        let unique: HashSet<Xid> = all.iter().copied().collect();
        assert_eq!(
            unique.len(),
            all.len(),
            "xids must be unique across threads"
        );
        // ...the counter ended exactly one past the highest issued...
        assert_eq!(mgr.next_xid(), all.iter().copied().max().unwrap() + 1);
        // ...and once quiescent, nothing is active and the horizon collapses.
        assert_eq!(mgr.active_count(), 0);
        assert_eq!(mgr.vacuum_horizon(), mgr.next_xid());
    }

    #[test]
    fn concurrent_reader_pins_horizon_under_writer_churn() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempdir().unwrap();
        let (_pool, _heap, wal) = setup(dir.path());
        let mgr = TransactionManager::new();
        let lock_mgr = LockManager::new();

        // Warm up so next_xid > 1, then take a long-lived reader registration.
        for _ in 0..4 {
            let x = mgr.begin(IsolationLevel::ReadCommitted, &wal).unwrap();
            mgr.commit(x, &wal, &lock_mgr).unwrap();
        }
        let shared = mgr.shared();
        let (snap, _self_xid, reg) = super::read_snapshot(&shared);
        let pinned = snap.xmin;

        let stop = AtomicBool::new(false);
        std::thread::scope(|s| {
            // Writers churn transactions while the reader registration is live.
            for _ in 0..4 {
                s.spawn(|| {
                    while !stop.load(Ordering::Relaxed) {
                        let x = mgr.begin(IsolationLevel::ReadCommitted, &wal).unwrap();
                        mgr.commit(x, &wal, &lock_mgr).unwrap();
                    }
                });
            }
            // Sampler: the horizon must NEVER pass the live reader's xmin, no
            // matter how many versions the writers churn behind it.
            s.spawn(|| {
                for _ in 0..20_000 {
                    assert!(
                        mgr.vacuum_horizon() <= pinned,
                        "vacuum horizon advanced past a live reader's snapshot"
                    );
                }
                stop.store(true, Ordering::Relaxed);
            });
        });

        // With writers stopped but the reader still live, the horizon is exactly
        // pinned; releasing the registration lets it finally advance.
        assert_eq!(mgr.vacuum_horizon(), pinned);
        drop(reg);
        assert!(mgr.vacuum_horizon() > pinned);
    }

    #[test]
    fn concurrent_lock_manager_admits_one_writer_per_row() {
        use crate::lockmgr::RecordId;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let lock_mgr = LockManager::new();
        let row = RecordId::row(7, 3);

        const THREADS: usize = 16;
        let wins = AtomicUsize::new(0);
        let conflicts = AtomicUsize::new(0);
        std::thread::scope(|s| {
            let (wins, conflicts, lm) = (&wins, &conflicts, &lock_mgr);
            for t in 0..THREADS {
                let xid = (t + 1) as Xid; // each racer is a distinct xid
                s.spawn(move || match lm.try_acquire_write(row, xid) {
                    Ok(()) => {
                        wins.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(DbError::WriteConflict { .. }) => {
                        conflicts.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => panic!("unexpected lock error: {e:?}"),
                });
            }
        });
        // Exactly one writer holds a row's write intent; the rest see a clean
        // WriteConflict — never two winners (which would be a lost update).
        assert_eq!(wins.load(Ordering::Relaxed), 1);
        assert_eq!(conflicts.load(Ordering::Relaxed), THREADS - 1);

        // The winner releasing lets a fresh writer take the row.
        let winner = lock_mgr.holder(row).unwrap();
        lock_mgr.release_all(winner);
        assert_eq!(lock_mgr.holder(row), None);
        assert!(lock_mgr.try_acquire_write(row, 999).is_ok());
    }
}
