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
}
