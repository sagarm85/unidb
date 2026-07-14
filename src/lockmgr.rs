// Lock manager (M1.b; upgraded to a real, blocking lock manager in P5.d).
//
// Keyed by (record_kind, record_id) per the architecture doc's stated design
// — only `RecordKind::Row` exists today; `Vector`/`GraphEdge`/`QueueEvent`
// variants land later without reshaping this key.
//
// ## Evolution
//
// * **M1.b** — a single `HashMap<RecordId, Xid>` of exclusive write intents,
//   abort-on-conflict, no waiting. That was correct for the single-writer
//   engine, where SI's first-committer-wins was the only policy needed.
// * **P5.c** — moved behind a `Mutex` so an `Arc<LockManager>` is shareable
//   across threads.
// * **P5.d (this)** — a real lock manager: **shared/exclusive modes**, a
//   **`WaitPolicy`** (`NoWait` keeps SI's immediate-abort first-committer-wins;
//   `Wait` blocks on a `Condvar` until the lock is free — the behavior a
//   blocking isolation level like READ COMMITTED wants), and **deadlock
//   detection** over a wait-for graph. When a blocking waiter would close a
//   cycle it is chosen as the victim and returned `DbError::Deadlock` instead
//   of hanging; the caller aborts (which releases its locks and unblocks the
//   rest). This is exercised for real once P5.e runs multiple writer threads;
//   the single-writer path keeps calling `try_acquire_write` (= Exclusive +
//   NoWait), so its behavior is unchanged.
//
// Locks are in-memory only, not WAL-logged — a concurrency-control mechanism
// for the currently-open transaction table, not a durability concern. Any
// in-flight transaction is implicitly aborted by recovery on restart, so there
// is nothing to recover here.
//
// Fairness is best-effort, not strict FIFO: a request is granted as soon as it
// is compatible with the current holders, so a steady stream of shared locks
// could in principle starve an exclusive waiter. Acceptable because the only
// caller today (`try_acquire_write`) takes exclusive locks exclusively, for
// which "grant when no other holder" is fair; strict queue fairness is noted as
// deferred tuning, not correctness.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Instant;

use crate::{
    error::{DbError, Result},
    format::{PageId, Xid},
    metrics::{AtomicHistogram, HistogramSnapshot},
};

/// Contention snapshot (item 21) — the lock-manager half of `stats()`.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct LockStats {
    /// Acquisitions that had to block at least once before being granted.
    pub waits: u64,
    /// Acquisitions aborted as the deadlock victim.
    pub deadlocks: u64,
    /// Wall-clock a blocked acquire spent parked before it was granted.
    pub wait: HistogramSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordKind {
    Row,
    /// Phantom lock for `PRIMARY KEY`/`UNIQUE` concurrent-insert serialization
    /// (item 35, Phase 2 inv. 3). Keyed by a hash of `(table, col, key_value)`;
    /// held from before `enforce_unique` through transaction commit, so a
    /// concurrent inserter racing the same key blocks here and sees the committed
    /// duplicate in its post-lock snapshot.
    UniqueKey,
    /// Phantom lock for FK referential-integrity serialization (item 36).
    /// Keyed by a hash of `(parent_table, ref_col, fk_value)`; acquired
    /// Exclusive by both the child inserter (before the parent-row lookup)
    /// and the parent deleter (before the RESTRICT scan), held through commit.
    /// Prevents the classic parent-delete / child-insert race: the first party
    /// to acquire the lock completes its check under a fresh snapshot; the
    /// second sees the committed state and either finds a child (RESTRICT) or
    /// finds no parent (FK violation), depending on which won.
    FkKey,
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

    /// Unique-key phantom lock: a `UniqueKey` record keyed by `hash` (a stable
    /// hash of `(table_name, col_name, encoded_key_value)` computed by the
    /// caller). Used by `exec_insert` to serialize concurrent inserters racing
    /// the same `PRIMARY KEY`/`UNIQUE` value (item 35, Phase 2 inv. 3).
    pub fn unique_key(hash: u64) -> Self {
        Self {
            kind: RecordKind::UniqueKey,
            id: hash,
        }
    }

    /// FK-key phantom lock: an `FkKey` record keyed by `hash` (a stable hash of
    /// `(parent_table, ref_col, fk_value)` computed by the caller). Used by
    /// `exec_insert`/`exec_update` (child-side) and `exec_delete` (parent-side
    /// RESTRICT) to prevent the parent-delete / child-insert race (item 36).
    pub fn fk_key(hash: u64) -> Self {
        Self {
            kind: RecordKind::FkKey,
            id: hash,
        }
    }
}

/// Lock strength. `Shared` locks are mutually compatible; `Exclusive` conflicts
/// with everything else. Readers under MVCC take no locks at all, so `Shared`
/// exists for future `SELECT ... FOR SHARE`-style intent; every current caller
/// takes `Exclusive`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    Shared,
    Exclusive,
}

/// What to do when a lock cannot be granted immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitPolicy {
    /// Fail immediately with `WriteConflict` — SI's first-committer-wins (D12).
    NoWait,
    /// Block until the lock becomes grantable, or `Deadlock` if waiting would
    /// close a wait-for cycle — the behavior a blocking level (RC) wants.
    Wait,
}

#[derive(Default)]
struct LockEntry {
    /// Current holders and the mode each holds.
    holders: HashMap<Xid, LockMode>,
    /// Xids blocked on this record, in arrival order. Used to keep the entry
    /// alive while anyone waits and for idempotent enqueue; grants are decided
    /// purely by holder compatibility (best-effort fairness — see module doc).
    queue: VecDeque<Xid>,
}

#[derive(Default)]
struct LockTable {
    locks: HashMap<RecordId, LockEntry>,
    /// Wait-for graph: `waits_for[w]` is the set of xids `w` is currently
    /// blocked behind. Maintained only while a `Wait` request is parked, and
    /// used for deadlock detection.
    waits_for: HashMap<Xid, HashSet<Xid>>,
}

impl LockTable {
    /// Can `xid` hold `mode` on `id` given the *other* xids' current holds?
    fn can_grant(&self, id: RecordId, xid: Xid, mode: LockMode) -> bool {
        match self.locks.get(&id) {
            None => true,
            Some(e) => match mode {
                LockMode::Exclusive => e.holders.keys().all(|&h| h == xid),
                LockMode::Shared => e
                    .holders
                    .iter()
                    .all(|(&h, &m)| h == xid || m == LockMode::Shared),
            },
        }
    }

    /// The *other* xids whose holds currently block `xid`'s `mode` request.
    fn blockers(&self, id: RecordId, xid: Xid, mode: LockMode) -> Vec<Xid> {
        match self.locks.get(&id) {
            None => Vec::new(),
            Some(e) => e
                .holders
                .iter()
                .filter(|(&h, &m)| {
                    h != xid
                        && match mode {
                            LockMode::Exclusive => true,
                            LockMode::Shared => m == LockMode::Exclusive,
                        }
                })
                .map(|(&h, _)| h)
                .collect(),
        }
    }

    /// Record `xid` as a holder of `id` at `mode` (upgrading S→X in place),
    /// and clear any waiting/blocked-on state it had.
    fn grant(&mut self, id: RecordId, xid: Xid, mode: LockMode) {
        let e = self.locks.entry(id).or_default();
        let slot = e.holders.entry(xid).or_insert(mode);
        if mode == LockMode::Exclusive {
            *slot = LockMode::Exclusive;
        }
        e.queue.retain(|&w| w != xid);
        self.waits_for.remove(&xid);
    }

    /// Add `xid` to `id`'s wait queue once (idempotent).
    fn enqueue(&mut self, id: RecordId, xid: Xid) {
        let e = self.locks.entry(id).or_default();
        if !e.queue.iter().any(|&w| w == xid) {
            e.queue.push_back(xid);
        }
    }

    /// Remove `xid` from `id`'s wait queue (on deadlock back-out).
    fn dequeue(&mut self, id: RecordId, xid: Xid) {
        if let Some(e) = self.locks.get_mut(&id) {
            e.queue.retain(|&w| w != xid);
        }
    }

    /// Would following wait-for edges from `start` lead back to `start`? A DFS
    /// over `waits_for`; the graph is acyclic before this call (every prior
    /// waiter that would have closed a cycle was aborted), so any cycle must
    /// pass through `start`.
    fn has_cycle(&self, start: Xid) -> bool {
        let mut stack: Vec<Xid> = self
            .waits_for
            .get(&start)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();
        let mut seen: HashSet<Xid> = HashSet::new();
        while let Some(n) = stack.pop() {
            if n == start {
                return true;
            }
            if !seen.insert(n) {
                continue;
            }
            if let Some(next) = self.waits_for.get(&n) {
                stack.extend(next.iter().copied());
            }
        }
        false
    }

    /// Drop every empty lock entry so the table doesn't grow without bound.
    fn gc(&mut self) {
        self.locks
            .retain(|_, e| !(e.holders.is_empty() && e.queue.is_empty()));
    }
}

pub struct LockManager {
    table: Mutex<LockTable>,
    cvar: Condvar,
    /// Contention observability (item 21). Lock-free atomics updated outside
    /// the deadlock-detection critical path: `waits` counts blocking acquires,
    /// `deadlocks` counts victim aborts, `wait_latency` records how long a
    /// parked waiter blocked before being granted. The no-wait (SI) path never
    /// touches these, so the default concurrent-write path pays nothing.
    waits: AtomicU64,
    deadlocks: AtomicU64,
    wait_latency: AtomicHistogram,
}

impl Default for LockManager {
    fn default() -> Self {
        Self::new()
    }
}

impl LockManager {
    pub fn new() -> Self {
        Self {
            table: Mutex::new(LockTable::default()),
            cvar: Condvar::new(),
            waits: AtomicU64::new(0),
            deadlocks: AtomicU64::new(0),
            wait_latency: AtomicHistogram::new(),
        }
    }

    /// Cold-path contention readout (item 21).
    pub fn lock_stats(&self) -> LockStats {
        LockStats {
            waits: self.waits.load(Ordering::Relaxed),
            deadlocks: self.deadlocks.load(Ordering::Relaxed),
            wait: self.wait_latency.snapshot(),
        }
    }

    /// Poison-safe access: a prior panic-while-locked leaves the table usable
    /// as-is (consistent with `txn.rs`/`wal.rs`).
    fn lock(&self) -> MutexGuard<'_, LockTable> {
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Acquire (or re-acquire / upgrade) `mode` on `id` for `xid`.
    ///
    /// `NoWait` returns `WriteConflict` the moment the lock is unavailable (SI).
    /// `Wait` blocks until the lock is grantable, or returns `Deadlock` if
    /// parking would close a wait-for cycle (the caller must then abort `xid`).
    pub fn acquire(
        &self,
        id: RecordId,
        xid: Xid,
        mode: LockMode,
        policy: WaitPolicy,
    ) -> Result<()> {
        let mut t = self.lock();
        // Wall-clock the *first* park to this grant (item 21 contention panel).
        // `None` until this acquire actually blocks, so an uncontended grant
        // records nothing and the lock-free counters stay untouched.
        let mut blocked_since: Option<Instant> = None;
        loop {
            if t.can_grant(id, xid, mode) {
                t.grant(id, xid, mode);
                // A grant can make queued (e.g. shared) waiters grantable too.
                self.cvar.notify_all();
                drop(t);
                if let Some(since) = blocked_since {
                    self.wait_latency.record(since.elapsed().as_micros() as u64);
                }
                return Ok(());
            }
            match policy {
                WaitPolicy::NoWait => {
                    let holder = t.blockers(id, xid, mode).first().copied().unwrap_or(xid);
                    return Err(DbError::WriteConflict { holder_xid: holder });
                }
                WaitPolicy::Wait => {
                    t.enqueue(id, xid);
                    let blockers: HashSet<Xid> = t.blockers(id, xid, mode).into_iter().collect();
                    t.waits_for.insert(xid, blockers);
                    if t.has_cycle(xid) {
                        // This waiter would close a cycle: abort it as the
                        // victim, backing out its queue/graph presence first.
                        t.dequeue(id, xid);
                        t.waits_for.remove(&xid);
                        t.gc();
                        self.cvar.notify_all();
                        self.deadlocks.fetch_add(1, Ordering::Relaxed); // item 21
                        tracing::info!(xid, "deadlock: chosen as victim");
                        return Err(DbError::Deadlock { xid });
                    }
                    // First time this acquire blocks: count the wait and start
                    // its clock (item 21). Subsequent spurious wake-ups reuse it.
                    if blocked_since.is_none() {
                        blocked_since = Some(Instant::now());
                        self.waits.fetch_add(1, Ordering::Relaxed);
                    }
                    t = self.cvar.wait(t).unwrap_or_else(|e| e.into_inner());
                }
            }
        }
    }

    /// Attempt an exclusive write intent on `id`, failing immediately on
    /// conflict — the M1.b/SI behavior, now a thin wrapper over [`Self::
    /// acquire`]. The single-writer path and every existing caller use this.
    pub fn try_acquire_write(&self, id: RecordId, xid: Xid) -> Result<()> {
        self.acquire(id, xid, LockMode::Exclusive, WaitPolicy::NoWait)
    }

    /// Acquire an exclusive lock on `id` with blocking wait + deadlock detection.
    /// Used by `exec_insert` for `UniqueKey` phantom locks: the caller blocks
    /// until the previous holder (the concurrent inserter racing the same key)
    /// commits or aborts, then re-checks visibility in a fresh snapshot.
    pub fn acquire_blocking(&self, id: RecordId, xid: Xid) -> Result<()> {
        self.acquire(id, xid, LockMode::Exclusive, WaitPolicy::Wait)
    }

    /// Release every lock held (and any wait parked) by `xid` — called on
    /// commit or abort — then wake blocked waiters so they can re-check.
    pub fn release_all(&self, xid: Xid) {
        {
            let mut t = self.lock();
            for e in t.locks.values_mut() {
                e.holders.remove(&xid);
                e.queue.retain(|&w| w != xid);
            }
            t.gc();
            t.waits_for.remove(&xid);
            // Stale edges pointing at the departed xid; live waiters rebuild
            // their blocker set on their next wake-up loop regardless.
            for blockers in t.waits_for.values_mut() {
                blockers.remove(&xid);
            }
        }
        self.cvar.notify_all();
    }

    /// Some xid currently holding `id`, if any (an exclusive holder is unique;
    /// with shared holders this returns an arbitrary one). Used by tests and
    /// introspection.
    pub fn holder(&self, id: RecordId) -> Option<Xid> {
        self.lock()
            .locks
            .get(&id)
            .and_then(|e| e.holders.keys().next().copied())
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

    // ── P5.d: modes, blocking waits, deadlock detection ──────────────────────

    #[test]
    fn shared_locks_coexist() {
        let lm = LockManager::new();
        let r = RecordId::row(3, 0);
        lm.acquire(r, 1, LockMode::Shared, WaitPolicy::Wait)
            .unwrap();
        // A second shared holder is compatible and never blocks.
        lm.acquire(r, 2, LockMode::Shared, WaitPolicy::Wait)
            .unwrap();
        // But an exclusive request cannot barge in while shared locks are held.
        let err = lm.acquire(r, 3, LockMode::Exclusive, WaitPolicy::NoWait);
        assert!(matches!(err, Err(DbError::WriteConflict { .. })));
    }

    #[test]
    fn exclusive_excludes_shared() {
        let lm = LockManager::new();
        let r = RecordId::row(4, 0);
        lm.acquire(r, 1, LockMode::Exclusive, WaitPolicy::NoWait)
            .unwrap();
        let err = lm.acquire(r, 2, LockMode::Shared, WaitPolicy::NoWait);
        assert!(matches!(err, Err(DbError::WriteConflict { holder_xid: 1 })));
    }

    #[test]
    fn blocking_wait_parks_then_grants_on_release() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        let lm = LockManager::new();
        let r = RecordId::row(5, 0);
        lm.try_acquire_write(r, 1).unwrap(); // xid1 holds it exclusively
        let got = AtomicBool::new(false);

        std::thread::scope(|s| {
            s.spawn(|| {
                // Blocks until xid1 releases, then acquires.
                lm.acquire(r, 2, LockMode::Exclusive, WaitPolicy::Wait)
                    .unwrap();
                got.store(true, Ordering::SeqCst);
            });

            // Give the waiter time to reach the parked state; it must NOT have
            // acquired while xid1 still holds the lock.
            std::thread::sleep(Duration::from_millis(80));
            assert!(
                !got.load(Ordering::SeqCst),
                "waiter acquired while lock was held"
            );

            // Releasing xid1 unblocks the waiter (scope join waits for it).
            lm.release_all(1);
        });

        assert!(got.load(Ordering::SeqCst));
        assert_eq!(lm.holder(r), Some(2));
    }

    #[test]
    fn deadlock_is_detected_and_one_victim_aborts() {
        let lm = LockManager::new();
        let a = RecordId::row(6, 0);
        let b = RecordId::row(6, 1);

        // xid1 holds a, xid2 holds b.
        lm.try_acquire_write(a, 1).unwrap();
        lm.try_acquire_write(b, 2).unwrap();

        // Each now reaches across for the other's lock: xid1 wants b, xid2
        // wants a — a cycle. Exactly one is chosen as the deadlock victim; it
        // aborts (releases its locks), which lets the other proceed.
        let (r1, r2) = std::thread::scope(|s| {
            let h1 = s.spawn(|| {
                let r = lm.acquire(b, 1, LockMode::Exclusive, WaitPolicy::Wait);
                if r.is_err() {
                    lm.release_all(1);
                }
                r
            });
            let h2 = s.spawn(|| {
                let r = lm.acquire(a, 2, LockMode::Exclusive, WaitPolicy::Wait);
                if r.is_err() {
                    lm.release_all(2);
                }
                r
            });
            (h1.join().unwrap(), h2.join().unwrap())
        });

        // Exactly one deadlocked; the other acquired.
        assert!(
            r1.is_err() ^ r2.is_err(),
            "exactly one transaction must be the victim (r1={r1:?}, r2={r2:?})"
        );
        let victim = if r1.is_err() { &r1 } else { &r2 };
        let survivor = if r1.is_err() { &r2 } else { &r1 };
        assert!(matches!(victim, Err(DbError::Deadlock { .. })));
        assert!(survivor.is_ok());
    }
}
