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
    bufferpool::{BufferPool, PageReader},
    concurrency_hooks::{on_read, on_write},
    error::{DbError, Result},
    format::{
        u16_from_le, u16_to_le, u32_from_le, u32_to_le, u64_from_le, u64_to_le, PageId, Xid,
        INVALID_PAGE_ID, PAGE_TYPE_HEAP,
    },
    lockmgr::{LockManager, RecordId},
    mvcc::{is_reclaimable, is_visible, Snapshot},
    page::{SlotState, SlottedPage},
    wal::Wal,
};

/// Stable row identifier: (page_id, slot). Identifies one physical tuple
/// version, not a logical row across versions — callers that need "the
/// current version of this row" re-resolve via a fresh scan/lookup rather
/// than dereferencing a RowId across statements (no cross-statement cursor
/// stability in M1).
// `serde::Serialize` (not gated behind the `server` feature — `serde` is
// already an unconditional core dependency, used by `Literal`/`CmpOp` etc.
// for the catalog's on-disk JSON blob; this is just a plain, harmless
// additive derive) so the M5 REST server can return a `RowId` directly as
// a JSON response body without a separate wrapper type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
pub struct RowId {
    pub page_id: PageId,
    pub slot: u16,
}

pub struct Heap {
    page_size: usize,
    /// Ordered list of page IDs belonging to this heap.
    pages: Vec<PageId>,
    /// Free-space map (P1.c): cached free bytes per known page, so
    /// `find_or_alloc_page` can pick a page that fits by comparing integers
    /// instead of *fetching* (copying 8 KiB of) every page — the old O(pages)
    /// per-insert cost that made the heap O(pages²) to fill. Populated as pages
    /// are touched and kept exact after every mutation that changes a page's
    /// free space (a hint only — never over-reports, so a chosen page always
    /// fits). For a `Heap` reconstructed via [`Self::from_pages`] it starts
    /// empty and is filled lazily (scanning from the end, append-locality).
    free_map: std::collections::HashMap<PageId, usize>,
}

impl Heap {
    pub fn new(page_size: usize) -> Self {
        Self {
            page_size,
            pages: Vec::new(),
            free_map: std::collections::HashMap::new(),
        }
    }

    /// Reconstruct a `Heap` handle over an already-populated set of pages
    /// (M1.c: the catalog persists each table's page list so `scan`/FSM
    /// work correctly after a reopen, rather than starting from an empty
    /// page list every time — see catalog.rs's `TableDef.pages`).
    pub fn from_pages(page_size: usize, pages: Vec<PageId>) -> Self {
        Self {
            page_size,
            pages,
            free_map: std::collections::HashMap::new(),
        }
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
        let mut page = pool.fetch_page_for_write(page_id, wal)?;
        // P1.a: full-page image before this page's first change of the interval.
        let prev_lsn = pool
            .maybe_log_fpi(page_id, wal, txn_id, begin_lsn)?
            .unwrap_or(begin_lsn);
        let slot = page.insert_versioned(data, xid, 0, prev)?;
        on_write(xid, RowId { page_id, slot });
        let redo = encode_insert_redo(xid, prev, data);
        let ins_lsn = wal.log_insert(txn_id, prev_lsn, page_id, slot, &redo)?;
        page.set_lsn(ins_lsn);
        pool.write_page(&page)?;
        self.note_free_space(page_id, &page); // P1.c: keep the FSM exact
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
    pub fn get<P: PageReader>(
        &self,
        row_id: RowId,
        snapshot: &Snapshot,
        self_xid: Xid,
        reader: &P,
    ) -> Result<Vec<u8>> {
        let page = reader.read_page(row_id.page_id)?;
        // A slot vacuum has reclaimed (DEAD/UNUSED, M10) resolves to "no
        // visible version" under any snapshot, not a hard error — a stale
        // secondary-index candidate pointing at a reclaimed slot is filtered
        // out here exactly like a superseded version.
        if !matches!(page.slot_state(row_id.slot), Ok(SlotState::Live)) {
            return Err(DbError::NoVisibleVersion {
                page_id: row_id.page_id,
                slot: row_id.slot,
            });
        }
        let th = page.tuple_header(row_id.slot)?;
        if is_visible(th.xmin, th.xmax, snapshot, self_xid) {
            on_read(self_xid, row_id);
            Ok(page.get(row_id.slot)?.to_vec())
        } else {
            Err(DbError::NoVisibleVersion {
                page_id: row_id.page_id,
                slot: row_id.slot,
            })
        }
    }

    /// Read a version's raw payload bytes ignoring MVCC visibility, as long as
    /// the slot is still `Live` (M10 vacuum / P3.a durable-index scrub: a
    /// reclaimable version is still physically present — slot `Live`, body
    /// intact — until `mark_dead`, so this recovers its indexed values in that
    /// window to scrub durable secondary indexes before the slot is reused).
    pub fn get_raw<P: PageReader>(&self, row_id: RowId, reader: &P) -> Result<Vec<u8>> {
        let page = reader.read_page(row_id.page_id)?;
        Ok(page.get(row_id.slot)?.to_vec())
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

        let mut old_page = pool.fetch_page_for_write(row_id.page_id, wal)?;
        let old_th = old_page.tuple_header(row_id.slot)?;
        if old_th.xmax != 0 {
            pool.unpin(row_id.page_id);
            wal.abort_mini_txn(txn_id, begin_lsn)?;
            return Err(DbError::WriteConflict {
                holder_xid: old_th.xmax,
            });
        }
        on_write(xid, row_id);
        // P1.a: full-page image of the old-version page before its xmax stamp.
        let xmax_prev = pool
            .maybe_log_fpi(row_id.page_id, wal, txn_id, begin_lsn)?
            .unwrap_or(begin_lsn);
        let xmax_lsn = wal.log_update(
            txn_id,
            xmax_prev,
            row_id.page_id,
            row_id.slot,
            &u64_to_le(xid),
            &u64_to_le(old_th.xmax),
        )?;
        old_page.set_xmax(row_id.slot, xid)?;
        old_page.set_lsn(xmax_lsn);
        pool.write_page(&old_page)?;
        pool.unpin(row_id.page_id);

        let mut new_page = pool.fetch_page_for_write(new_page_id, wal)?;
        // P1.a: full-page image of the new-version page before its insert. A
        // no-op if this is the same page as the old version (already covered).
        let ins_prev = pool
            .maybe_log_fpi(new_page_id, wal, txn_id, xmax_lsn)?
            .unwrap_or(xmax_lsn);
        let prev = Some((row_id.page_id, row_id.slot));
        let new_slot = new_page.insert_versioned(new_data, xid, 0, prev)?;
        let insert_redo = encode_insert_redo(xid, prev, new_data);
        let ins_lsn = wal.log_insert(txn_id, ins_prev, new_page_id, new_slot, &insert_redo)?;
        new_page.set_lsn(ins_lsn);
        pool.write_page(&new_page)?;
        self.note_free_space(new_page_id, &new_page); // P1.c: keep the FSM exact
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
        let mut page = pool.fetch_page_for_write(row_id.page_id, wal)?;
        let th = page.tuple_header(row_id.slot)?;
        if th.xmax != 0 {
            pool.unpin(row_id.page_id);
            wal.abort_mini_txn(txn_id, begin_lsn)?;
            return Err(DbError::WriteConflict {
                holder_xid: th.xmax,
            });
        }
        on_write(xid, row_id);
        // P1.a: full-page image before this page's first change of the interval.
        let prev_lsn = pool
            .maybe_log_fpi(row_id.page_id, wal, txn_id, begin_lsn)?
            .unwrap_or(begin_lsn);
        let lsn = wal.log_update(
            txn_id,
            prev_lsn,
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
        let mut page = pool.fetch_page_for_write(page_id, wal)?;
        let old_xmax = page.tuple_header(slot)?.xmax;
        // P1.a: full-page image before this page's first change of the interval.
        let prev_lsn = pool
            .maybe_log_fpi(page_id, wal, txn_id, begin_lsn)?
            .unwrap_or(begin_lsn);
        let lsn = wal.log_update(
            txn_id,
            prev_lsn,
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
        let mut page = pool.fetch_page_for_write(page_id, wal)?;
        // P1.a: full-page image before this page's first change of the interval.
        let prev_lsn = pool
            .maybe_log_fpi(page_id, wal, txn_id, begin_lsn)?
            .unwrap_or(begin_lsn);
        let lsn = wal.log_update(
            txn_id,
            prev_lsn,
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
    pub fn scan<P: PageReader>(
        &self,
        snapshot: &Snapshot,
        self_xid: Xid,
        reader: &P,
    ) -> Result<Vec<(RowId, Vec<u8>)>> {
        let mut out = Vec::new();
        for &page_id in &self.pages {
            let page = reader.read_page(page_id)?;
            let sc = page.slot_count_pub();
            for slot in 0..sc {
                // Skip line pointers a vacuum has reclaimed (DEAD/UNUSED,
                // M10) — they carry no resolvable tuple body.
                if !matches!(page.slot_state(slot), Ok(SlotState::Live)) {
                    continue;
                }
                let th = page.tuple_header(slot)?;
                if is_visible(th.xmin, th.xmax, snapshot, self_xid) {
                    let row_id = RowId { page_id, slot };
                    on_read(self_xid, row_id);
                    out.push((row_id, page.get(slot)?.to_vec()));
                }
            }
        }
        Ok(out)
    }

    // ── FSM ──────────────────────────────────────────────────────────────────

    /// Record a page's current free space in the FSM (P1.c). Call after any
    /// mutation that changes free space, with the page in hand.
    fn note_free_space(&mut self, page_id: PageId, page: &SlottedPage) {
        self.free_map.insert(page_id, page.free_space());
    }

    /// Find a page with room for `needed` bytes, or allocate a new one (P1.c —
    /// real free-space map). Fast path: the cached `free_map` answers "which
    /// page fits?" with integer comparisons and **no page fetch**. Only pages
    /// whose free space is still *unknown* (a freshly reconstructed
    /// `from_pages` heap) are fetched — and those from the end backward
    /// (append locality), stopping at the first fit and caching every probe —
    /// so the common append case costs at most one fetch instead of O(pages).
    fn find_or_alloc_page(
        &mut self,
        needed: usize,
        pool: &mut BufferPool,
        wal: &mut Wal,
    ) -> Result<PageId> {
        // 1. Known pages that fit — pure integer comparison, no fetch.
        for &pid in &self.pages {
            if self.free_map.get(&pid).is_some_and(|&free| free >= needed) {
                return Ok(pid);
            }
        }
        // 2. Unknown pages: probe from the end (most recent = most likely to
        //    have room), caching each result so a later probe is free.
        let unknown: Vec<PageId> = self
            .pages
            .iter()
            .rev()
            .filter(|pid| !self.free_map.contains_key(pid))
            .copied()
            .collect();
        for pid in unknown {
            let page = pool.fetch_page_for_write(pid, wal)?;
            let free = page.free_space();
            pool.unpin(pid);
            self.free_map.insert(pid, free);
            if free >= needed {
                return Ok(pid);
            }
        }
        // 3. Nothing fits — allocate a fresh page.
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
        self.note_free_space(pid, &page);
        tracing::debug!(page_id = pid, "heap page allocated");
        Ok(pid)
    }

    // ── M10: vacuum / garbage collection ─────────────────────────────────────

    /// Every reclaimable tuple version in this heap under `horizon` (M10.b): a
    /// raw *physical* scan (not MVCC-filtered) of every LIVE slot, keeping the
    /// ones whose committed `xmax` is below the horizon (`mvcc::is_reclaimable`
    /// — the inverse of `is_visible`). These are the versions no live or future
    /// snapshot can ever see again.
    pub fn collect_reclaimable<P: PageReader>(
        &self,
        horizon: Xid,
        reader: &P,
    ) -> Result<Vec<RowId>> {
        let mut out = Vec::new();
        for &page_id in &self.pages {
            let page = reader.read_page(page_id)?;
            let sc = page.slot_count_pub();
            for slot in 0..sc {
                if !matches!(page.slot_state(slot), Ok(SlotState::Live)) {
                    continue;
                }
                let th = page.tuple_header(slot)?;
                if is_reclaimable(th.xmax, horizon) {
                    out.push(RowId { page_id, slot });
                }
            }
        }
        Ok(out)
    }

    /// Mark one reclaimable version's line pointer DEAD (M10.b): the slot stops
    /// resolving, but its pointer is retained and NOT reusable — a stale
    /// secondary-index entry may still reference `(page, slot)` until vacuum's
    /// index pass promotes it (M10.c/d). WAL-logged as a redo-only, idempotent
    /// mini-txn (D2/D5); no undo, since re-freeing already-dead space on
    /// recovery replay is a no-op.
    pub fn mark_dead(&mut self, row_id: RowId, pool: &mut BufferPool, wal: &mut Wal) -> Result<()> {
        let (txn_id, begin_lsn) = wal.begin_mini_txn()?;
        let mut page = pool.fetch_page_for_write(row_id.page_id, wal)?;
        // P1.a: full-page image before this page's first change of the interval
        // (mark_dead is an incremental slot mutation, so it needs torn-page
        // protection just like an INSERT/UPDATE).
        let prev_lsn = pool
            .maybe_log_fpi(row_id.page_id, wal, txn_id, begin_lsn)?
            .unwrap_or(begin_lsn);
        let lsn = wal.log_vacuum(txn_id, prev_lsn, row_id.page_id, row_id.slot, &[])?;
        page.mark_dead(row_id.slot)?;
        page.set_lsn(lsn);
        pool.write_page(&page)?;
        pool.unpin(row_id.page_id);
        wal.commit_mini_txn(txn_id, lsn)?;
        Ok(())
    }

    /// Compact one page (M10.d): physically drop the bodies of DEAD/UNUSED
    /// slots, coalesce the freed space, and promote every reclaimed slot to
    /// UNUSED (reusable). WAL-logged redo-only as a full compacted page image
    /// (`slot == u16::MAX`), idempotent on replay via the page LSN check.
    /// Returns the number of bytes reclaimed. **Only** call this after the
    /// index-clean pass (M10.c), since it makes reclaimed slots reusable.
    pub fn compact_page(
        &mut self,
        page_id: PageId,
        pool: &mut BufferPool,
        wal: &mut Wal,
    ) -> Result<usize> {
        let (txn_id, begin_lsn) = wal.begin_mini_txn()?;
        let mut page = pool.fetch_page_for_write(page_id, wal)?;
        let reclaimed = page.compact();
        // Log the compacted bytes *before* stamping the record's LSN — recovery
        // reconstructs the image and re-stamps `r.lsn` itself (see recovery.rs).
        let image = page.as_bytes().to_vec();
        let lsn = wal.log_vacuum(txn_id, begin_lsn, page_id, u16::MAX, &image)?;
        page.set_lsn(lsn);
        pool.write_page(&page)?;
        self.note_free_space(page_id, &page); // P1.c: compaction freed space
        pool.unpin(page_id);
        wal.commit_mini_txn(txn_id, lsn)?;
        // P1.a: this WAL_VACUUM record already carries a full clean page image
        // (its own torn-page protection), so no separate FPI is needed for a
        // later modification of this page in the same interval.
        pool.mark_fpi_logged(page_id);
        Ok(reclaimed)
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
        let data = heap.get(rid, &snap, xid, &pool).unwrap();
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
            heap.get(rid, &snap_b, 2, &pool),
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
        assert_eq!(heap.get(rid, &snap_after, 2, &pool).unwrap(), b"hello");
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
            heap.get(rid, &snap, xid, &pool),
            Err(DbError::NoVisibleVersion { .. })
        ));
        assert_eq!(heap.get(new_rid, &snap, xid, &pool).unwrap(), b"new_value");
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
            heap.get(rid, &snap_before_update, xid_b, &pool).unwrap(),
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
            heap.get(rid, &snap_after, 2, &pool),
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
            .scan(&snap, xid, &pool)
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
            heap.get(rid, &snap, xid, &pool),
            Err(DbError::NoVisibleVersion { .. })
        ));
        // And to a later, unrelated snapshot too.
        let snap_after = Snapshot::new(2, 2, vec![]);
        assert!(matches!(
            heap.get(rid, &snap_after, 2, &pool),
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
        assert_eq!(heap.get(rid, &snap, xid, &pool).unwrap(), b"row");
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
        assert_eq!(heap.get(r1, &snap, xid, &pool).unwrap(), b"row1");
        assert_eq!(heap.get(r2, &snap, xid, &pool).unwrap(), b"row2");
        assert_eq!(heap.get(r3, &snap, xid, &pool).unwrap(), b"row3");
    }

    // ── M10: vacuum ──────────────────────────────────────────────────────────

    #[test]
    fn collect_reclaimable_finds_only_committed_deleted_below_horizon() {
        let dir = tempdir().unwrap();
        let (mut heap, mut pool, mut wal, mut lock_mgr) = setup(dir.path());
        let xid = 1;
        let live = heap.insert(b"live", xid, &mut pool, &mut wal).unwrap();
        let dead = heap.insert(b"dead", xid, &mut pool, &mut wal).unwrap();
        heap.delete(dead, xid, &mut pool, &mut wal, &mut lock_mgr)
            .unwrap();

        // Horizon below the deleter (xid=1): nothing reclaimable yet.
        assert!(heap.collect_reclaimable(1, &pool).unwrap().is_empty());
        // Horizon above the deleter: the deleted version is reclaimable, the
        // live one is not.
        let reclaimable = heap.collect_reclaimable(5, &pool).unwrap();
        assert_eq!(reclaimable, vec![dead]);
        assert!(!reclaimable.contains(&live));
    }

    #[test]
    fn mark_dead_removes_version_and_survives_visibility() {
        let dir = tempdir().unwrap();
        let (mut heap, mut pool, mut wal, mut lock_mgr) = setup(dir.path());
        let xid = 1;
        let keep = heap.insert(b"keep", xid, &mut pool, &mut wal).unwrap();
        let gone = heap.insert(b"gone", xid, &mut pool, &mut wal).unwrap();
        heap.delete(gone, xid, &mut pool, &mut wal, &mut lock_mgr)
            .unwrap();

        for rid in heap.collect_reclaimable(5, &pool).unwrap() {
            heap.mark_dead(rid, &mut pool, &mut wal).unwrap();
        }
        // The kept row is still visible; the vacuumed one is gone from scan.
        let snap = Snapshot::new(5, 5, vec![]);
        let rows: Vec<Vec<u8>> = heap
            .scan(&snap, 5, &pool)
            .unwrap()
            .into_iter()
            .map(|(_, d)| d)
            .collect();
        assert_eq!(rows, vec![b"keep".to_vec()]);
        assert_eq!(heap.get(keep, &snap, 5, &pool).unwrap(), b"keep");
    }

    #[test]
    fn compact_page_reclaims_space() {
        let dir = tempdir().unwrap();
        let (mut heap, mut pool, mut wal, mut lock_mgr) = setup(dir.path());
        let xid = 1;
        let big = vec![b'z'; 400];
        let dead = heap.insert(&big, xid, &mut pool, &mut wal).unwrap();
        heap.insert(b"survivor", xid, &mut pool, &mut wal).unwrap();
        heap.delete(dead, xid, &mut pool, &mut wal, &mut lock_mgr)
            .unwrap();
        heap.mark_dead(dead, &mut pool, &mut wal).unwrap();

        let reclaimed = heap
            .compact_page(dead.page_id, &mut pool, &mut wal)
            .unwrap();
        assert!(reclaimed >= 400, "compaction must reclaim the dead body");

        let snap = Snapshot::new(5, 5, vec![]);
        let rows: Vec<Vec<u8>> = heap
            .scan(&snap, 5, &pool)
            .unwrap()
            .into_iter()
            .map(|(_, d)| d)
            .collect();
        assert_eq!(rows, vec![b"survivor".to_vec()]);
    }

    /// P1.c: many small inserts pack into as few pages as fit (the FSM points
    /// each insert at a page with room), and every row stays readable.
    #[test]
    fn fsm_packs_small_rows_and_reuses_pages() {
        let dir = tempdir().unwrap();
        let (mut heap, mut pool, mut wal, _lock) = setup(dir.path());
        let xid = 1;
        let mut rids = Vec::new();
        for i in 0u32..200 {
            rids.push(
                heap.insert(&i.to_le_bytes(), xid, &mut pool, &mut wal)
                    .unwrap(),
            );
        }
        // 200 tiny rows fit in only a handful of 8 KiB pages — the FSM must
        // keep filling a page with room rather than allocating one per row.
        assert!(
            heap.page_ids().len() < 10,
            "small rows should pack tightly, got {} pages",
            heap.page_ids().len()
        );
        // Every row is still correct.
        let snap = solo_snapshot(xid);
        for (i, rid) in rids.iter().enumerate() {
            assert_eq!(
                heap.get(*rid, &snap, xid, &pool).unwrap(),
                (i as u32).to_le_bytes()
            );
        }
    }

    /// P1.c: space freed by vacuum compaction is recorded in the FSM and
    /// reused by a later insert rather than growing the heap.
    #[test]
    fn fsm_reuses_compacted_space() {
        let dir = tempdir().unwrap();
        let (mut heap, mut pool, mut wal, mut lock) = setup(dir.path());
        let xid = 1;
        let big = vec![b'x'; 4000]; // ~half a page
        let dead = heap.insert(&big, xid, &mut pool, &mut wal).unwrap();
        heap.delete(dead, xid, &mut pool, &mut wal, &mut lock)
            .unwrap();
        heap.mark_dead(dead, &mut pool, &mut wal).unwrap();
        heap.compact_page(dead.page_id, &mut pool, &mut wal)
            .unwrap();
        let pages_before = heap.page_ids().len();
        // A row that fits in the reclaimed space must reuse the compacted page.
        let reused = heap
            .insert(&vec![b'y'; 3000], xid, &mut pool, &mut wal)
            .unwrap();
        assert_eq!(
            reused.page_id, dead.page_id,
            "insert must reuse freed space"
        );
        assert_eq!(heap.page_ids().len(), pages_before, "heap must not grow");
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
