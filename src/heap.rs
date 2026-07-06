// Single-table heap with MVCC versioning (M1, D4).
//
// INSERT creates a brand-new live version (xmin = xid). UPDATE is no longer
// in-place (M0's in-place update is replaced, as D4 promised the on-disk
// format would allow without a rewrite): it inserts a new version chained to
// the old one via `prev_page`/`prev_slot`, then stamps the old version's
// xmax. DELETE stamps xmax on the current version — it no longer physically
// removes the slot; M0's physical `page.delete()` is now purely a future
// vacuum operation, not used by any M1 code path. Dead versions accumulate
// with no reclamation in M1 (documented tech debt: safe, but a throughput/
// storage cost for update/delete-heavy workloads until a vacuum milestone).
//
// Each heap-level mutation still maps to WAL mini-transactions (D2). UPDATE
// now spans two page mutations (new-version insert + old-version xmax stamp)
// under ONE mini-txn bracket, so it remains a single atomic redo/undo unit.
//
// WAL_INSERT's redo payload is `[xmin:8][prev_page:4][prev_slot:2][payload]`
// rather than bare payload bytes, so that (a) redo replay during recovery can
// reconstruct the exact tuple header, and (b) recovery's user-transaction
// undo pass (recovery.rs) can identify which xid a mutation belongs to by
// decoding xmin, without needing a separate xid field in the WAL wire format.
// An xmax-stamp mutation's (DELETE, or UPDATE's old-version half) redo
// payload is simply the new xmax value (8 bytes) — which *is* the acting
// transaction's xid, so no extra encoding is needed there.

use crate::{
    bufferpool::BufferPool,
    concurrency_hooks::{on_read, on_write},
    error::{DbError, Result},
    format::{
        u16_from_le, u16_to_le, u32_from_le, u32_to_le, u64_from_le, u64_to_le, PageId, Xid,
        INVALID_PAGE_ID, PAGE_TYPE_HEAP,
    },
    lockmgr::{LockManager, RecordId},
    mvcc::{is_visible, Snapshot},
    page::SlottedPage,
    wal::Wal,
};

/// Stable row identifier: (page_id, slot). Identifies one physical tuple
/// version, not a logical row across versions — callers that need "the
/// current version of this row" re-resolve via a fresh scan/lookup rather
/// than dereferencing a RowId across statements (no cross-statement cursor
/// stability in M1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowId {
    pub page_id: PageId,
    pub slot: u16,
}

pub struct Heap {
    page_size: usize,
    /// Ordered list of page IDs belonging to this heap.
    pages: Vec<PageId>,
}

impl Heap {
    pub fn new(page_size: usize) -> Self {
        Self {
            page_size,
            pages: Vec::new(),
        }
    }

    /// Reconstruct a `Heap` handle over an already-populated set of pages
    /// (M1.c: the catalog persists each table's page list so `scan`/FSM
    /// work correctly after a reopen, rather than starting from an empty
    /// page list every time — see catalog.rs's `TableDef.pages`).
    pub fn from_pages(page_size: usize, pages: Vec<PageId>) -> Self {
        Self { page_size, pages }
    }

    /// The heap's current page list, so callers (the SQL executor) can
    /// detect growth and persist the updated list back to the catalog.
    pub fn page_ids(&self) -> &[PageId] {
        &self.pages
    }

    /// INSERT: create a brand-new live row, owned by `xid`.
    pub fn insert(
        &mut self,
        data: &[u8],
        xid: Xid,
        pool: &mut BufferPool,
        wal: &mut Wal,
    ) -> Result<RowId> {
        self.insert_version(data, xid, None, pool, wal)
    }

    fn insert_version(
        &mut self,
        data: &[u8],
        xid: Xid,
        prev: Option<(PageId, u16)>,
        pool: &mut BufferPool,
        wal: &mut Wal,
    ) -> Result<RowId> {
        let needed = crate::page::SLOT_SIZE + crate::page::TUPLE_HEADER_SIZE + data.len();
        let page_id = self.find_or_alloc_page(needed, pool, wal)?;

        let (txn_id, begin_lsn) = wal.begin_mini_txn()?;
        let mut page = pool.fetch_page(page_id)?;
        let slot = page.insert_versioned(data, xid, 0, prev)?;
        on_write(xid, RowId { page_id, slot });
        let redo = encode_insert_redo(xid, prev, data);
        let ins_lsn = wal.log_insert(txn_id, begin_lsn, page_id, slot, &redo)?;
        page.set_lsn(ins_lsn);
        pool.write_page(&page)?;
        pool.unpin(page_id);
        wal.commit_mini_txn(txn_id, ins_lsn)?;
        Ok(RowId { page_id, slot })
    }

    /// Read the specific tuple version at `row_id` if it is visible under
    /// `snapshot`. `row_id` identifies one physical version, not a logical
    /// row across versions — there is no cross-statement RowId stability in
    /// M1 (D4/M1 plan): once a version is superseded or deleted, its old
    /// RowId simply stops resolving, even for the transaction that
    /// superseded it. Callers needing "the current version of this row"
    /// re-resolve via `scan()` or the row_id an `insert`/`update` returned,
    /// not by re-using a stale one.
    pub fn get(
        &self,
        row_id: RowId,
        snapshot: &Snapshot,
        self_xid: Xid,
        pool: &mut BufferPool,
    ) -> Result<Vec<u8>> {
        let page = pool.fetch_page(row_id.page_id)?;
        let th = page.tuple_header(row_id.slot)?;
        let visible = is_visible(th.xmin, th.xmax, snapshot, self_xid);
        let data = if visible {
            on_read(self_xid, row_id);
            Some(page.get(row_id.slot)?.to_vec())
        } else {
            None
        };
        pool.unpin(row_id.page_id);
        data.ok_or(DbError::NoVisibleVersion {
            page_id: row_id.page_id,
            slot: row_id.slot,
        })
    }

    /// UPDATE: insert a new version chained to `row_id`, then stamp the old
    /// version's xmax = `xid`. Both mutations happen under one mini-txn
    /// bracket, so the update remains a single atomic redo/undo unit (D2).
    /// Returns the new version's RowId.
    ///
    /// Two distinct conflict checks (M1.b, D12): (1) `lock_mgr` catches
    /// another *currently active* transaction racing for this row — fails
    /// fast, no waiting, per SI's simple abort-on-conflict path; (2) the
    /// `xmax != 0` check catches a row already superseded by a transaction
    /// that has since *committed and released its lock* — a distinct
    /// failure mode the lock table alone can't see once the holder is gone.
    pub fn update(
        &mut self,
        row_id: RowId,
        new_data: &[u8],
        xid: Xid,
        pool: &mut BufferPool,
        wal: &mut Wal,
        lock_mgr: &mut LockManager,
    ) -> Result<RowId> {
        lock_mgr.try_acquire_write(RecordId::row(row_id.page_id, row_id.slot), xid)?;

        let needed = crate::page::SLOT_SIZE + crate::page::TUPLE_HEADER_SIZE + new_data.len();
        let new_page_id = self.find_or_alloc_page(needed, pool, wal)?;

        let (txn_id, begin_lsn) = wal.begin_mini_txn()?;

        let mut old_page = pool.fetch_page(row_id.page_id)?;
        let old_th = old_page.tuple_header(row_id.slot)?;
        if old_th.xmax != 0 {
            pool.unpin(row_id.page_id);
            wal.abort_mini_txn(txn_id, begin_lsn)?;
            return Err(DbError::WriteConflict {
                holder_xid: old_th.xmax,
            });
        }
        on_write(xid, row_id);
        let xmax_lsn = wal.log_update(
            txn_id,
            begin_lsn,
            row_id.page_id,
            row_id.slot,
            &u64_to_le(xid),
            &u64_to_le(old_th.xmax),
        )?;
        old_page.set_xmax(row_id.slot, xid)?;
        old_page.set_lsn(xmax_lsn);
        pool.write_page(&old_page)?;
        pool.unpin(row_id.page_id);

        let mut new_page = pool.fetch_page(new_page_id)?;
        let prev = Some((row_id.page_id, row_id.slot));
        let new_slot = new_page.insert_versioned(new_data, xid, 0, prev)?;
        let insert_redo = encode_insert_redo(xid, prev, new_data);
        let ins_lsn = wal.log_insert(txn_id, xmax_lsn, new_page_id, new_slot, &insert_redo)?;
        new_page.set_lsn(ins_lsn);
        pool.write_page(&new_page)?;
        pool.unpin(new_page_id);

        wal.commit_mini_txn(txn_id, ins_lsn)?;
        Ok(RowId {
            page_id: new_page_id,
            slot: new_slot,
        })
    }

    /// DELETE: stamp xmax = `xid` on the current version. Physical removal
    /// is deferred to a future vacuum operation (not implemented in M1). See
    /// `update`'s doc comment for why both a lock-manager check and an
    /// `xmax != 0` check are needed.
    pub fn delete(
        &mut self,
        row_id: RowId,
        xid: Xid,
        pool: &mut BufferPool,
        wal: &mut Wal,
        lock_mgr: &mut LockManager,
    ) -> Result<()> {
        lock_mgr.try_acquire_write(RecordId::row(row_id.page_id, row_id.slot), xid)?;

        let (txn_id, begin_lsn) = wal.begin_mini_txn()?;
        let mut page = pool.fetch_page(row_id.page_id)?;
        let th = page.tuple_header(row_id.slot)?;
        if th.xmax != 0 {
            pool.unpin(row_id.page_id);
            wal.abort_mini_txn(txn_id, begin_lsn)?;
            return Err(DbError::WriteConflict {
                holder_xid: th.xmax,
            });
        }
        on_write(xid, row_id);
        let lsn = wal.log_update(
            txn_id,
            begin_lsn,
            row_id.page_id,
            row_id.slot,
            &u64_to_le(xid),
            &u64_to_le(th.xmax),
        )?;
        page.set_xmax(row_id.slot, xid)?;
        page.set_lsn(lsn);
        pool.write_page(&page)?;
        pool.unpin(row_id.page_id);
        wal.commit_mini_txn(txn_id, lsn)?;
        Ok(())
    }

    /// Reverse a previously-applied xmax stamp (DELETE, or UPDATE's
    /// old-version half): revert back to 0 (live). Used by transaction
    /// abort/rollback (txn.rs) and by recovery's incomplete-user-txn undo
    /// pass (recovery.rs).
    pub fn undo_xmax_stamp(
        &mut self,
        page_id: PageId,
        slot: u16,
        pool: &mut BufferPool,
        wal: &mut Wal,
    ) -> Result<()> {
        let (txn_id, begin_lsn) = wal.begin_mini_txn()?;
        let mut page = pool.fetch_page(page_id)?;
        let old_xmax = page.tuple_header(slot)?.xmax;
        let lsn = wal.log_update(
            txn_id,
            begin_lsn,
            page_id,
            slot,
            &u64_to_le(0),
            &u64_to_le(old_xmax),
        )?;
        page.set_xmax(slot, 0)?;
        page.set_lsn(lsn);
        pool.write_page(&page)?;
        pool.unpin(page_id);
        wal.commit_mini_txn(txn_id, lsn)?;
        Ok(())
    }

    /// Reverse a previously-applied INSERT (or UPDATE's new-version half):
    /// self-stamp the tuple's own xmax so it becomes permanently invisible.
    /// This reuses `mvcc::is_visible`'s existing committed/active
    /// distinction instead of requiring a separate "aborted" tuple state:
    /// once `xid` is no longer active, the tuple looks exactly like an
    /// ordinary row that was inserted and later deleted by the same
    /// (by-then-finished) transaction.
    pub fn undo_insert(
        &mut self,
        page_id: PageId,
        slot: u16,
        xid: Xid,
        pool: &mut BufferPool,
        wal: &mut Wal,
    ) -> Result<()> {
        let (txn_id, begin_lsn) = wal.begin_mini_txn()?;
        let mut page = pool.fetch_page(page_id)?;
        let lsn = wal.log_update(
            txn_id,
            begin_lsn,
            page_id,
            slot,
            &u64_to_le(xid),
            &u64_to_le(0),
        )?;
        page.set_xmax(slot, xid)?;
        page.set_lsn(lsn);
        pool.write_page(&page)?;
        pool.unpin(page_id);
        wal.commit_mini_txn(txn_id, lsn)?;
        Ok(())
    }

    /// Sequential scan: every row visible under `snapshot`. Used by the SQL
    /// executor's table scan (M1.c) and available now for hand-written
    /// interleaved-transaction tests.
    pub fn scan(
        &self,
        snapshot: &Snapshot,
        self_xid: Xid,
        pool: &mut BufferPool,
    ) -> Result<Vec<(RowId, Vec<u8>)>> {
        let mut out = Vec::new();
        for &page_id in &self.pages {
            let page = pool.fetch_page(page_id)?;
            let sc = page.slot_count_pub();
            for slot in 0..sc {
                let th = page.tuple_header(slot)?;
                if is_visible(th.xmin, th.xmax, snapshot, self_xid) {
                    let row_id = RowId { page_id, slot };
                    on_read(self_xid, row_id);
                    out.push((row_id, page.get(slot)?.to_vec()));
                }
            }
            pool.unpin(page_id);
        }
        Ok(out)
    }

    // ── FSM ──────────────────────────────────────────────────────────────────

    fn find_or_alloc_page(
        &mut self,
        needed: usize,
        pool: &mut BufferPool,
        wal: &mut Wal,
    ) -> Result<PageId> {
        for &pid in &self.pages {
            let page = pool.fetch_page(pid)?;
            let free = page.free_space();
            pool.unpin(pid);
            if free >= needed {
                return Ok(pid);
            }
        }
        self.alloc_heap_page(pool, wal)
    }

    fn alloc_heap_page(&mut self, pool: &mut BufferPool, wal: &mut Wal) -> Result<PageId> {
        let pid = pool.alloc_page()?;
        let (txn_id, begin_lsn) = wal.begin_mini_txn()?;
        let alloc_lsn = wal.log_insert(txn_id, begin_lsn, pid, u16::MAX, &[])?;
        let mut page = SlottedPage::new(pid, PAGE_TYPE_HEAP, self.page_size);
        page.set_lsn(alloc_lsn);
        wal.commit_mini_txn(txn_id, alloc_lsn)?;
        pool.write_page(&page)?;
        pool.unpin(pid);
        self.pages.push(pid);
        tracing::debug!(page_id = pid, "heap page allocated");
        Ok(pid)
    }
}

/// Encode a versioned-INSERT WAL redo payload: `[xmin:8][prev_page:4][prev_slot:2][payload]`.
pub fn encode_insert_redo(xmin: Xid, prev: Option<(PageId, u16)>, payload: &[u8]) -> Vec<u8> {
    let (prev_page, prev_slot) = prev.unwrap_or((INVALID_PAGE_ID, 0));
    let mut buf = Vec::with_capacity(14 + payload.len());
    buf.extend_from_slice(&u64_to_le(xmin));
    buf.extend_from_slice(&u32_to_le(prev_page));
    buf.extend_from_slice(&u16_to_le(prev_slot));
    buf.extend_from_slice(payload);
    buf
}

/// `(xmin, prev-version pointer, payload)` decoded from a versioned-INSERT
/// WAL redo payload.
type InsertRedo<'a> = (Xid, Option<(PageId, u16)>, &'a [u8]);

/// Decode a versioned-INSERT WAL redo payload. Returns `(xmin, prev, payload)`.
pub fn decode_insert_redo(buf: &[u8]) -> Result<InsertRedo<'_>> {
    if buf.len() < 14 {
        return Err(DbError::WalCorrupt { lsn: 0 });
    }
    let xmin = u64_from_le(buf[0..8].try_into().unwrap());
    let prev_page = u32_from_le(buf[8..12].try_into().unwrap());
    let prev_slot = u16_from_le(buf[12..14].try_into().unwrap());
    let payload = &buf[14..];
    let prev = if prev_page == INVALID_PAGE_ID {
        None
    } else {
        Some((prev_page, prev_slot))
    };
    Ok((xmin, prev, payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bufferpool::BufferPool;
    use crate::format::{DEFAULT_PAGE_SIZE, INVALID_LSN};
    use crate::mvcc::Snapshot;
    use crate::wal::Wal;
    use tempfile::tempdir;

    fn setup(dir: &std::path::Path) -> (Heap, BufferPool, Wal, LockManager) {
        let pool = BufferPool::open(&dir.join("data.db"), DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        let wal = Wal::open(&dir.join("db.wal"), INVALID_LSN).unwrap();
        let heap = Heap::new(DEFAULT_PAGE_SIZE as usize);
        (heap, pool, wal, LockManager::new())
    }

    /// A snapshot that sees everything committed strictly before `xid`, with
    /// no other active transactions — enough for single-transaction tests.
    fn solo_snapshot(xid: Xid) -> Snapshot {
        Snapshot::new(xid, xid + 1, vec![xid])
    }

    #[test]
    fn insert_and_get() {
        let dir = tempdir().unwrap();
        let (mut heap, mut pool, mut wal, _lock_mgr) = setup(dir.path());
        let xid = 1;
        let rid = heap.insert(b"hello", xid, &mut pool, &mut wal).unwrap();
        let snap = solo_snapshot(xid);
        let data = heap.get(rid, &snap, xid, &mut pool).unwrap();
        assert_eq!(data, b"hello");
    }

    #[test]
    fn insert_invisible_to_other_active_txn() {
        let dir = tempdir().unwrap();
        let (mut heap, mut pool, mut wal, _lock_mgr) = setup(dir.path());
        let xid_a = 1;
        let rid = heap.insert(b"hello", xid_a, &mut pool, &mut wal).unwrap();
        // xid_b's snapshot considers xid_a still active.
        let snap_b = Snapshot::new(xid_a, 3, vec![xid_a]);
        assert!(matches!(
            heap.get(rid, &snap_b, 2, &mut pool),
            Err(DbError::NoVisibleVersion { .. })
        ));
    }

    #[test]
    fn insert_visible_once_committed() {
        let dir = tempdir().unwrap();
        let (mut heap, mut pool, mut wal, _lock_mgr) = setup(dir.path());
        let xid_a = 1;
        let rid = heap.insert(b"hello", xid_a, &mut pool, &mut wal).unwrap();
        // Fresh snapshot after xid_a "committed": xid_a no longer active.
        let snap_after = Snapshot::new(2, 2, vec![]);
        assert_eq!(heap.get(rid, &snap_after, 2, &mut pool).unwrap(), b"hello");
    }

    #[test]
    fn update_creates_new_version_and_hides_old() {
        let dir = tempdir().unwrap();
        let (mut heap, mut pool, mut wal, mut lock_mgr) = setup(dir.path());
        let xid = 1;
        let rid = heap.insert(b"old_value", xid, &mut pool, &mut wal).unwrap();
        let new_rid = heap
            .update(rid, b"new_value", xid, &mut pool, &mut wal, &mut lock_mgr)
            .unwrap();
        let snap = solo_snapshot(xid);
        // The old RowId is a specific physical version, now superseded by
        // xid's own update — it is not resolvable anymore, even to xid
        // itself (no cross-statement RowId stability across an UPDATE;
        // callers re-resolve via the RowId `update` returned, or a scan).
        assert!(matches!(
            heap.get(rid, &snap, xid, &mut pool),
            Err(DbError::NoVisibleVersion { .. })
        ));
        assert_eq!(
            heap.get(new_rid, &snap, xid, &mut pool).unwrap(),
            b"new_value"
        );
    }

    #[test]
    fn other_txn_sees_old_version_until_update_commits() {
        let dir = tempdir().unwrap();
        let (mut heap, mut pool, mut wal, mut lock_mgr) = setup(dir.path());
        let xid_a = 1;
        let rid = heap.insert(b"v1", xid_a, &mut pool, &mut wal).unwrap();
        // xid_b begins (RR) right after xid_a committed: fixed snapshot
        // sees everything below xid 2 as committed, nothing at/above as
        // committed yet.
        let xid_b = 2;
        let snap_before_update = Snapshot::new(xid_b, xid_b, vec![]);
        // A later transaction, xid_c, updates the row after xid_b's
        // snapshot was already fixed.
        let xid_c = 3;
        let _new_rid = heap
            .update(rid, b"v2", xid_c, &mut pool, &mut wal, &mut lock_mgr)
            .unwrap();
        // xid_b's fixed snapshot predates xid_c's update, so it still sees v1.
        assert_eq!(
            heap.get(rid, &snap_before_update, xid_b, &mut pool)
                .unwrap(),
            b"v1"
        );
    }

    #[test]
    fn delete_hides_row_from_later_snapshot() {
        let dir = tempdir().unwrap();
        let (mut heap, mut pool, mut wal, mut lock_mgr) = setup(dir.path());
        let xid = 1;
        let rid = heap.insert(b"to_delete", xid, &mut pool, &mut wal).unwrap();
        heap.delete(rid, xid, &mut pool, &mut wal, &mut lock_mgr)
            .unwrap();
        let snap_after = Snapshot::new(2, 2, vec![]);
        assert!(matches!(
            heap.get(rid, &snap_after, 2, &mut pool),
            Err(DbError::NoVisibleVersion { .. })
        ));
    }

    #[test]
    fn concurrent_update_conflict_is_rejected() {
        let dir = tempdir().unwrap();
        let (mut heap, mut pool, mut wal, mut lock_mgr) = setup(dir.path());
        let xid_a = 1;
        let rid = heap.insert(b"row", xid_a, &mut pool, &mut wal).unwrap();
        heap.update(rid, b"a-wins", xid_a, &mut pool, &mut wal, &mut lock_mgr)
            .unwrap();
        // A second writer trying to update the now-superseded old version
        // hits the xmax already set by xid_a.
        let xid_b = 2;
        let err = heap.update(rid, b"b-loses", xid_b, &mut pool, &mut wal, &mut lock_mgr);
        assert!(matches!(err, Err(DbError::WriteConflict { holder_xid }) if holder_xid == xid_a));
    }

    #[test]
    fn scan_returns_only_visible_rows() {
        let dir = tempdir().unwrap();
        let (mut heap, mut pool, mut wal, mut lock_mgr) = setup(dir.path());
        let xid = 1;
        heap.insert(b"row1", xid, &mut pool, &mut wal).unwrap();
        let r2 = heap.insert(b"row2", xid, &mut pool, &mut wal).unwrap();
        heap.delete(r2, xid, &mut pool, &mut wal, &mut lock_mgr)
            .unwrap();
        let snap = solo_snapshot(xid);
        let rows: Vec<Vec<u8>> = heap
            .scan(&snap, xid, &mut pool)
            .unwrap()
            .into_iter()
            .map(|(_, d)| d)
            .collect();
        assert_eq!(rows, vec![b"row1".to_vec()]);
    }

    #[test]
    fn undo_insert_makes_row_permanently_invisible() {
        let dir = tempdir().unwrap();
        let (mut heap, mut pool, mut wal, _lock_mgr) = setup(dir.path());
        let xid = 1;
        let rid = heap.insert(b"oops", xid, &mut pool, &mut wal).unwrap();
        heap.undo_insert(rid.page_id, rid.slot, xid, &mut pool, &mut wal)
            .unwrap();
        // Even to xid itself, the row is gone.
        let snap = solo_snapshot(xid);
        assert!(matches!(
            heap.get(rid, &snap, xid, &mut pool),
            Err(DbError::NoVisibleVersion { .. })
        ));
        // And to a later, unrelated snapshot too.
        let snap_after = Snapshot::new(2, 2, vec![]);
        assert!(matches!(
            heap.get(rid, &snap_after, 2, &mut pool),
            Err(DbError::NoVisibleVersion { .. })
        ));
    }

    #[test]
    fn undo_xmax_stamp_restores_visibility() {
        let dir = tempdir().unwrap();
        let (mut heap, mut pool, mut wal, mut lock_mgr) = setup(dir.path());
        let xid = 1;
        let rid = heap.insert(b"row", xid, &mut pool, &mut wal).unwrap();
        heap.delete(rid, xid, &mut pool, &mut wal, &mut lock_mgr)
            .unwrap();
        heap.undo_xmax_stamp(rid.page_id, rid.slot, &mut pool, &mut wal)
            .unwrap();
        let snap = solo_snapshot(xid);
        assert_eq!(heap.get(rid, &snap, xid, &mut pool).unwrap(), b"row");
    }

    #[test]
    fn multiple_rows() {
        let dir = tempdir().unwrap();
        let (mut heap, mut pool, mut wal, _lock_mgr) = setup(dir.path());
        let xid = 1;
        let r1 = heap.insert(b"row1", xid, &mut pool, &mut wal).unwrap();
        let r2 = heap.insert(b"row2", xid, &mut pool, &mut wal).unwrap();
        let r3 = heap.insert(b"row3", xid, &mut pool, &mut wal).unwrap();
        let snap = solo_snapshot(xid);
        assert_eq!(heap.get(r1, &snap, xid, &mut pool).unwrap(), b"row1");
        assert_eq!(heap.get(r2, &snap, xid, &mut pool).unwrap(), b"row2");
        assert_eq!(heap.get(r3, &snap, xid, &mut pool).unwrap(), b"row3");
    }

    #[test]
    fn insert_redo_round_trip() {
        let redo = encode_insert_redo(42, Some((7, 3)), b"payload");
        let (xmin, prev, payload) = decode_insert_redo(&redo).unwrap();
        assert_eq!(xmin, 42);
        assert_eq!(prev, Some((7, 3)));
        assert_eq!(payload, b"payload");
    }

    #[test]
    fn insert_redo_round_trip_no_prev() {
        let redo = encode_insert_redo(1, None, b"x");
        let (xmin, prev, payload) = decode_insert_redo(&redo).unwrap();
        assert_eq!(xmin, 1);
        assert_eq!(prev, None);
        assert_eq!(payload, b"x");
    }
}
