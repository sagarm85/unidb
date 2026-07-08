// MVCC visibility rules (M1, D10–D12). Pure logic, no I/O — the transaction
// manager (txn.rs) decides *when* to construct a Snapshot (fresh per
// statement for READ COMMITTED, once at BEGIN for REPEATABLE READ/SI); this
// module only decides whether a given tuple version is visible under one.
//
// Because aborted transactions are undone physically (task: undo_transaction
// in txn.rs replays WAL undo payloads immediately on abort), a tuple that
// still exists on disk with a given xmin/xmax was never written by an
// aborted transaction — only "committed" and "still active" need
// distinguishing here, not "aborted".

use crate::format::Xid;

/// A point-in-time view of which transactions were committed/active,
/// used to decide tuple visibility.
///
/// - `xmin`: smallest xid still active when the snapshot was taken. Any xid
///   below this is guaranteed committed before the snapshot.
/// - `xmax`: `next_xid` at snapshot time. Any xid at or above this did not
///   exist yet and is invisible (a "future" transaction).
/// - `active_xids`: xids in `[xmin, xmax)` that were in-progress
///   (uncommitted) at snapshot time. Membership is fixed at snapshot
///   construction — it does not change even if those transactions commit
///   later, which is what gives REPEATABLE READ its stability.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub xmin: Xid,
    pub xmax: Xid,
    pub active_xids: Vec<Xid>,
}

impl Snapshot {
    pub fn new(xmin: Xid, xmax: Xid, active_xids: Vec<Xid>) -> Self {
        Self {
            xmin,
            xmax,
            active_xids,
        }
    }

    /// Was `xid` committed as of this snapshot's point in time?
    fn is_committed_at_snapshot(&self, xid: Xid) -> bool {
        if xid == 0 {
            return false; // INVALID_XID — never a real committed transaction
        }
        if xid >= self.xmax {
            return false; // did not exist yet when the snapshot was taken
        }
        if xid < self.xmin {
            return true; // guaranteed committed before any snapshot-time-active txn began
        }
        !self.active_xids.contains(&xid)
    }
}

/// Is a tuple with the given `(xmin, xmax)` visible to `self_xid` under `snapshot`?
///
/// A transaction always sees its own writes regardless of commit status.
/// Otherwise: the inserter must be committed-as-of-snapshot, and the tuple
/// must not have been deleted/superseded by a deleter that is also
/// committed-as-of-snapshot (or by `self_xid` itself).
pub fn is_visible(tuple_xmin: Xid, tuple_xmax: Xid, snapshot: &Snapshot, self_xid: Xid) -> bool {
    let inserter_visible = tuple_xmin == self_xid || snapshot.is_committed_at_snapshot(tuple_xmin);
    if !inserter_visible {
        return false;
    }
    if tuple_xmax == 0 {
        return true; // live — never deleted or superseded
    }
    let deleter_visible = tuple_xmax == self_xid || snapshot.is_committed_at_snapshot(tuple_xmax);
    !deleter_visible
}

/// Is a tuple version with the given `xmax` reclaimable by vacuum under
/// `horizon` (M10)? This is the deliberate inverse of [`is_visible`] for the
/// "dead to everyone" case: a version is reclaimable iff it has been
/// superseded/deleted (`xmax != 0`) by a transaction that every current and
/// future snapshot must see as committed (`xmax < horizon`, where `horizon` is
/// the oldest live snapshot's `xmin` — see `TransactionManager::
/// vacuum_horizon`).
///
/// Two facts make this sound with only `xmax`:
/// - A non-zero on-disk `xmax` always denotes a *committed* deleter: aborts
///   are physically undone (`Heap::undo_xmax_stamp` reverts xmax to 0), so an
///   aborted delete never leaves a lingering xmax (see this module's header).
/// - `xmax < horizon` means `xmax` is below every live transaction's xid
///   (each live xid `>= horizon`), so no live transaction *is* the deleter and
///   every snapshot treats it as committed-in-the-past.
///
/// A live tip (`xmax == 0`) is never reclaimable. Cross-checked against
/// `is_visible` in this module's tests.
pub fn is_reclaimable(tuple_xmax: Xid, horizon: Xid) -> bool {
    tuple_xmax != 0 && tuple_xmax < horizon
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_uncommitted_insert_is_visible_to_self() {
        // xid 5 is still active (in the snapshot's own active set), but a
        // transaction always sees its own writes.
        let snap = Snapshot::new(3, 10, vec![5]);
        assert!(is_visible(5, 0, &snap, 5));
    }

    #[test]
    fn other_txns_uncommitted_insert_is_invisible() {
        let snap = Snapshot::new(3, 10, vec![5]);
        assert!(!is_visible(5, 0, &snap, 6));
    }

    #[test]
    fn committed_insert_before_snapshot_xmin_is_visible() {
        let snap = Snapshot::new(10, 20, vec![]);
        assert!(is_visible(2, 0, &snap, 99));
    }

    #[test]
    fn committed_insert_in_range_but_not_active_is_visible() {
        // xid 12 is in [xmin, xmax) but not in active_xids => committed.
        let snap = Snapshot::new(10, 20, vec![15]);
        assert!(is_visible(12, 0, &snap, 99));
    }

    #[test]
    fn insert_from_future_xid_is_invisible() {
        let snap = Snapshot::new(3, 10, vec![]);
        assert!(!is_visible(10, 0, &snap, 99));
        assert!(!is_visible(50, 0, &snap, 99));
    }

    #[test]
    fn own_uncommitted_delete_hides_row_from_self() {
        let snap = Snapshot::new(3, 10, vec![5]);
        // self_xid=5 deleted a row it can see (xmin committed before snapshot).
        assert!(!is_visible(1, 5, &snap, 5));
    }

    #[test]
    fn uncommitted_delete_by_other_txn_does_not_hide_row() {
        // Row deleted by still-active xid 5, from xid 6's point of view the
        // delete hasn't happened yet.
        let snap = Snapshot::new(3, 10, vec![5]);
        assert!(is_visible(1, 5, &snap, 6));
    }

    #[test]
    fn committed_delete_hides_row_from_later_snapshot() {
        // Deleter xid 7 is committed as of this later snapshot.
        let snap = Snapshot::new(10, 20, vec![]);
        assert!(!is_visible(1, 7, &snap, 99));
    }

    #[test]
    fn repeatable_read_snapshot_does_not_see_delete_that_committed_after_it() {
        // Snapshot taken while xid 7 (the eventual deleter) was still active
        // and in-range: RR/SI's fixed snapshot must keep treating it as
        // uncommitted even though it has since committed in real time.
        let snap = Snapshot::new(3, 8, vec![7]);
        assert!(is_visible(1, 7, &snap, 99));
    }

    #[test]
    fn insert_and_delete_by_same_txn_visible_only_before_its_own_delete() {
        let snap = Snapshot::new(3, 10, vec![5]);
        // Row inserted and deleted by the same still-active xid 5: invisible
        // to xid 5 itself (it deleted its own row)...
        assert!(!is_visible(5, 5, &snap, 5));
        // ...and invisible to everyone else too (insert not yet committed).
        assert!(!is_visible(5, 5, &snap, 6));
    }

    // ── M10.a: reclaimable is the inverse of is_visible below the horizon ────

    #[test]
    fn live_tip_is_never_reclaimable() {
        assert!(!is_reclaimable(0, 100));
    }

    #[test]
    fn deleted_below_horizon_is_reclaimable() {
        assert!(is_reclaimable(7, 100));
    }

    #[test]
    fn deleted_at_or_above_horizon_is_not_reclaimable() {
        // xmax == horizon: a snapshot taken exactly at the horizon might still
        // be that deleting transaction's own, so it is not yet safe.
        assert!(!is_reclaimable(100, 100));
        assert!(!is_reclaimable(150, 100));
    }

    /// The load-bearing cross-check the M10 plan asks for: whenever a version
    /// is reclaimable under `horizon`, the snapshot that sees the *most* rows
    /// as live at the horizon (`xmin == xmax == horizon`, no active txns —
    /// the boundary a future reader could hold) must agree it is invisible.
    /// And a live tip must be visible to that same snapshot.
    #[test]
    fn reclaimable_implies_invisible_at_horizon_snapshot() {
        let horizon = 100;
        let at_horizon = Snapshot::new(horizon, horizon, vec![]);
        for xmin in [1u64, 5, 50, 99] {
            for xmax in [0u64, 1, 50, 99, 100, 200] {
                if is_reclaimable(xmax, horizon) {
                    assert!(
                        !is_visible(xmin, xmax, &at_horizon, 0),
                        "reclaimable ({xmin},{xmax}) must be invisible at horizon"
                    );
                }
            }
            // A live version (xmax == 0) inserted below the horizon is visible
            // and never reclaimable.
            assert!(is_visible(xmin, 0, &at_horizon, 0));
            assert!(!is_reclaimable(0, horizon));
        }
    }
}
