// Recovery (D1 — steal + no-force, ARIES-style):
//   1. Read control file → get checkpoint_lsn.
//   2. Redo all committed mini-transactions from checkpoint_lsn onward.
//   3. Undo any incomplete mini-transactions (no COMMIT record).
//
// Never panics on a bad page or corrupt WAL record — detects and reports (D1).
// Structured logging throughout (D13).

use std::{collections::HashSet, path::Path};

use crate::{
    btree_index::redo_index_insert,
    bufferpool::BufferPool,
    control::{self, ControlData},
    error::{DbError, Result},
    format::{
        u16_from_le, u32_from_le, u64_from_le, Xid, HOT_NEXT_NONE, INVALID_LSN, PAGE_TYPE_HEAP,
        WAL_ABORT, WAL_BEGIN, WAL_CHECKPOINT, WAL_COMMIT, WAL_DELETE, WAL_FPI, WAL_HOT_UPDATE,
        WAL_HOT_XPAGE_BATCH, WAL_HOT_XPAGE_HEAD, WAL_INDEX, WAL_INDEX_INSERT, WAL_INSERT,
        WAL_INSERT_BATCH, WAL_TXN_ABORT, WAL_TXN_BEGIN, WAL_TXN_COMMIT, WAL_UPDATE, WAL_VACUUM,
        WAL_XMAX_BATCH,
    },
    heap::{decode_insert_redo, RowId},
    page::SlottedPage,
    wal::{Wal, WalRecord},
};

pub struct RecoveryStats {
    pub records_scanned: usize,
    pub records_redone: usize,
    pub records_undone: usize,
    pub incomplete_txns: usize,
    /// User transactions (M1) that began but never reached `WAL_TXN_COMMIT`
    /// — undone even though their individual statements' mini-txns may have
    /// each committed durably (D2's per-statement unit vs. M1's
    /// multi-statement unit are tracked independently; see txn.rs).
    pub incomplete_user_txns: usize,
}

pub fn recover(
    control_path: &Path,
    data_path: &Path,
    wal_path: &Path,
    page_size: usize,
    pool_capacity: usize,
) -> Result<(ControlData, RecoveryStats)> {
    tracing::info!("recovery: starting");

    let control = control::read(control_path)?;
    let ckpt_lsn = control.checkpoint_lsn;
    tracing::info!(checkpoint_lsn = ckpt_lsn, "recovery: control file read");

    let records = Wal::scan_file(wal_path)?;
    tracing::info!(count = records.len(), "recovery: WAL records scanned");

    // Only process records at or after the checkpoint LSN.
    let relevant: Vec<&WalRecord> = records
        .iter()
        .filter(|r| r.lsn >= ckpt_lsn || ckpt_lsn == INVALID_LSN)
        .collect();

    // ── analysis pass: find committed and incomplete mini-txns ───────────────
    let mut committed: HashSet<u64> = HashSet::new();
    let mut aborted: HashSet<u64> = HashSet::new();
    let mut started: HashSet<u64> = HashSet::new();

    for r in &relevant {
        match r.rec_type {
            WAL_BEGIN => {
                started.insert(r.mini_txn_id);
            }
            WAL_COMMIT => {
                committed.insert(r.mini_txn_id);
            }
            WAL_ABORT => {
                aborted.insert(r.mini_txn_id);
            }
            WAL_CHECKPOINT => {}
            _ => {}
        }
    }

    let incomplete: HashSet<u64> = started
        .difference(&committed)
        .filter(|id| !aborted.contains(id))
        .copied()
        .collect();

    tracing::info!(
        committed = committed.len(),
        incomplete = incomplete.len(),
        "recovery: analysis pass complete"
    );

    let pool = BufferPool::open(data_path, page_size, pool_capacity)?;

    // Advance the pool's durable-WAL frontier (D5) to the tail of the on-disk
    // log before replaying. Every record we are about to redo is *already
    // durable* (it is in the persisted WAL being scanned), so the redo/undo
    // passes may freely flush dirty pages back to steal frames — otherwise, with
    // the frontier left at `INVALID_LSN`, `find_victim` would refuse to evict any
    // dirty redo page and a recovery whose working set exceeds `pool_capacity`
    // (the small-pool / large-transaction case commit-time fsync's C2 makes
    // ordinary) would exhaust the pool and silently drop the rest of the redo.
    let durable_frontier = records.iter().map(|r| r.lsn).max().unwrap_or(INVALID_LSN);
    pool.set_durable_wal_lsn(durable_frontier);

    let mut stats = RecoveryStats {
        records_scanned: relevant.len(),
        records_redone: 0,
        records_undone: 0,
        incomplete_txns: incomplete.len(),
        incomplete_user_txns: 0,
    };

    // ── redo pass: replay committed mutations ────────────────────────────────
    for r in &relevant {
        if r.rec_type == WAL_BEGIN
            || r.rec_type == WAL_COMMIT
            || r.rec_type == WAL_ABORT
            || r.rec_type == WAL_CHECKPOINT
        {
            continue;
        }
        if !committed.contains(&r.mini_txn_id) {
            continue;
        }

        match redo_record(r, &pool, page_size) {
            Ok(()) => stats.records_redone += 1,
            Err(e) => {
                tracing::warn!(lsn = r.lsn, error = %e, "recovery: redo skipped");
            }
        }
    }

    // ── undo pass: reverse incomplete mini-txns ──────────────────────────────
    // Collect undo targets in reverse LSN order.
    let mut undo_records: Vec<&WalRecord> = relevant
        .iter()
        .filter(|r| incomplete.contains(&r.mini_txn_id))
        .filter(|r| {
            r.rec_type == WAL_INSERT
                || r.rec_type == WAL_INSERT_BATCH
                || r.rec_type == WAL_UPDATE
                || r.rec_type == WAL_DELETE
                || r.rec_type == WAL_XMAX_BATCH
                || r.rec_type == WAL_HOT_UPDATE
                || r.rec_type == WAL_HOT_XPAGE_HEAD
                || r.rec_type == WAL_HOT_XPAGE_BATCH
        })
        .copied()
        .collect();
    undo_records.sort_by_key(|r| std::cmp::Reverse(r.lsn));

    for r in undo_records {
        match undo_record(r, &pool, page_size) {
            Ok(()) => stats.records_undone += 1,
            Err(e) => {
                tracing::warn!(lsn = r.lsn, error = %e, "recovery: undo skipped");
            }
        }
    }

    // ── M1: undo incomplete user transactions ─────────────────────────────
    // A user transaction (xid) is a sequence of mini-txns tied together by
    // WAL_TXN_BEGIN/COMMIT/ABORT (txn.rs). Its individual statements may
    // each have already committed (and been redone above) — but if the
    // transaction as a whole never reached WAL_TXN_COMMIT, all of its
    // effects must be undone regardless. Ownership of a mutation is
    // recovered from the tuple bytes themselves (xmin for INSERT, the new
    // xmax value for an xmax-stamp UPDATE — see heap.rs), not a separate
    // xid field in the WAL wire format.
    let mut user_started: HashSet<Xid> = HashSet::new();
    let mut user_committed: HashSet<Xid> = HashSet::new();
    let mut user_aborted: HashSet<Xid> = HashSet::new();
    for r in &relevant {
        match r.rec_type {
            WAL_TXN_BEGIN => {
                user_started.insert(r.mini_txn_id);
            }
            WAL_TXN_COMMIT => {
                user_committed.insert(r.mini_txn_id);
            }
            WAL_TXN_ABORT => {
                user_aborted.insert(r.mini_txn_id);
            }
            _ => {}
        }
    }
    let incomplete_user_txns: HashSet<Xid> = user_started
        .difference(&user_committed)
        .filter(|xid| !user_aborted.contains(xid))
        .copied()
        .collect();
    stats.incomplete_user_txns = incomplete_user_txns.len();

    if !incomplete_user_txns.is_empty() {
        // Phase 1: revert xmax stamps this xid applied to pre-existing rows
        // (DELETE, or an UPDATE's old-version half) back to 0 (live).
        //
        // Handles both the per-row WAL_UPDATE path and the batched
        // WAL_XMAX_BATCH path (item 56 Step 3).
        for r in relevant.iter().filter(|r| {
            (r.rec_type == WAL_UPDATE
                || r.rec_type == WAL_XMAX_BATCH
                || r.rec_type == WAL_HOT_UPDATE
                || r.rec_type == WAL_HOT_XPAGE_HEAD
                || r.rec_type == WAL_HOT_XPAGE_BATCH)
                && committed.contains(&r.mini_txn_id)
        }) {
            if r.rec_type == WAL_UPDATE {
                if let Ok(new_xmax) = decode_xmax(&r.redo) {
                    if incomplete_user_txns.contains(&new_xmax) {
                        let mut page = fetch_or_create(&pool, r.page_id, page_size)?;
                        page.set_xmax(r.slot, 0)?;
                        page.recompute_crc();
                        pool.write_page(&page)?;
                        pool.unpin(r.page_id);
                        stats.records_undone += 1;
                    }
                }
            } else if r.rec_type == WAL_XMAX_BATCH {
                // WAL_XMAX_BATCH: redo[0..8] = xid, redo[8..10] = n_slots,
                // redo[10..] = slot array.
                if let Ok((new_xmax, slots)) = decode_xmax_batch_redo(&r.redo) {
                    if incomplete_user_txns.contains(&new_xmax) {
                        let mut page = fetch_or_create(&pool, r.page_id, page_size)?;
                        for slot in slots {
                            match page.set_xmax(slot, 0) {
                                Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                                Err(e) => return Err(e),
                            }
                        }
                        page.recompute_crc();
                        pool.write_page(&page)?;
                        pool.unpin(r.page_id);
                        stats.records_undone += 1;
                    }
                }
            } else if r.rec_type == WAL_HOT_UPDATE {
                // WAL_HOT_UPDATE: redo[0..8] = xid, redo[8..10] = old_slot,
                // redo[10..12] = new_slot. Undo: clear hot_next + xmax on old slot,
                // delete new slot.
                if let Ok((new_xmax, old_slot, new_slot, _)) = decode_hot_update_redo(&r.redo) {
                    if incomplete_user_txns.contains(&new_xmax) {
                        let mut page = fetch_or_create(&pool, r.page_id, page_size)?;
                        // Delete new slot (phase 1 — make new version invisible).
                        match page.delete(new_slot) {
                            Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                            Err(e) => return Err(e),
                        }
                        // Clear hot_next on old slot (phase 2a).
                        match page.set_hot_next(old_slot, HOT_NEXT_NONE) {
                            Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                            Err(e) => return Err(e),
                        }
                        // Clear xmax on old slot (phase 2b — restore to live).
                        match page.set_xmax(old_slot, 0) {
                            Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                            Err(e) => return Err(e),
                        }
                        page.recompute_crc();
                        pool.write_page(&page)?;
                        pool.unpin(r.page_id);
                        stats.records_undone += 1;
                    }
                }
            } else if r.rec_type == WAL_HOT_XPAGE_HEAD {
                // WAL_HOT_XPAGE_HEAD (item 71): redo[0..8] = xid, redo[8..10] = old_slot,
                // redo[10..14] = new_page_id, redo[14..16] = new_slot.
                // undo[0..2] = old_slot, undo[2..6] = saved_prev_page, undo[6..8] = saved_prev_slot.
                //
                // Phase 1 (old page): restore prev_page/prev_slot + clear hot_next +
                //   clear xmax on old_slot.
                // Phase 2 (new page): handled by Phase 2 of the WAL_INSERT loop below
                //   (xmin self-stamp makes new version permanently invisible).
                if r.redo.len() >= 8 && r.undo.len() >= 8 {
                    let xid = u64_from_le(r.redo[0..8].try_into().unwrap());
                    if incomplete_user_txns.contains(&xid) {
                        let old_slot = u16_from_le(r.undo[0..2].try_into().unwrap());
                        let saved_prev_page = u32_from_le(r.undo[2..6].try_into().unwrap());
                        let saved_prev_slot = u16_from_le(r.undo[6..8].try_into().unwrap());
                        let mut page = fetch_or_create(&pool, r.page_id, page_size)?;
                        match page.restore_prev_and_hot_next(
                            old_slot,
                            saved_prev_page,
                            saved_prev_slot,
                        ) {
                            Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                            Err(e) => return Err(e),
                        }
                        match page.set_xmax(old_slot, 0) {
                            Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                            Err(e) => return Err(e),
                        }
                        page.recompute_crc();
                        pool.write_page(&page)?;
                        pool.unpin(r.page_id);
                        stats.records_undone += 1;
                    }
                }
            } else {
                // WAL_HOT_XPAGE_BATCH (item 80): xid is in redo[0..8] (same
                // layout as WAL_HOT_XPAGE_HEAD); undo payload is N×(old_slot +
                // saved_prev_page + saved_prev_slot).
                if r.redo.len() >= 8 {
                    let xid = u64_from_le(r.redo[0..8].try_into().unwrap());
                    if incomplete_user_txns.contains(&xid) {
                        if let Ok(entries) = decode_hot_xpage_batch_undo_entries(&r.undo) {
                            let mut page = fetch_or_create(&pool, r.page_id, page_size)?;
                            for (old_slot, saved_prev_page, saved_prev_slot) in entries {
                                match page.restore_prev_and_hot_next(
                                    old_slot,
                                    saved_prev_page,
                                    saved_prev_slot,
                                ) {
                                    Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                                    Err(e) => return Err(e),
                                }
                                match page.set_xmax(old_slot, 0) {
                                    Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                                    Err(e) => return Err(e),
                                }
                            }
                            page.recompute_crc();
                            pool.write_page(&page)?;
                            pool.unpin(r.page_id);
                            stats.records_undone += 1;
                        }
                    }
                }
            }
        }
        // Phase 2: force-self-stamp every row this xid inserted (INSERT or
        // WAL_INSERT_BATCH — the new-version half of a cross-page HOT update)
        // so it is permanently invisible.
        // Runs *after* phase 1 so that a row this xid both inserted and
        // later re-superseded within its own transaction ends up dead
        // (self-stamped) rather than incorrectly live (reverted to 0 by an
        // earlier phase-1 stamp targeting the same slot).
        //
        // Note: for WAL_HOT_UPDATE, the new version's xmin IS the acting xid;
        // it was already handled in the loop above (we deleted the slot directly
        // instead of self-stamping, which is equivalent). No separate entry here.
        for r in relevant.iter().filter(|r| {
            (r.rec_type == WAL_INSERT && r.slot != u16::MAX || r.rec_type == WAL_INSERT_BATCH)
                && committed.contains(&r.mini_txn_id)
        }) {
            if r.rec_type == WAL_INSERT {
                if let Ok((xmin, _, _)) = decode_insert_redo(&r.redo) {
                    if incomplete_user_txns.contains(&xmin) {
                        let mut page = fetch_or_create(&pool, r.page_id, page_size)?;
                        page.set_xmax(r.slot, xmin)?;
                        page.recompute_crc();
                        pool.write_page(&page)?;
                        pool.unpin(r.page_id);
                        stats.records_undone += 1;
                    }
                }
            } else {
                // WAL_INSERT_BATCH: xmin is in redo[0..8]; self-stamp all slots.
                if r.redo.len() >= 8 {
                    let xmin = u64_from_le(r.redo[0..8].try_into().unwrap());
                    if incomplete_user_txns.contains(&xmin) {
                        if let Ok(batch_rows) = decode_insert_batch_redo(&r.redo) {
                            let mut page = fetch_or_create(&pool, r.page_id, page_size)?;
                            for (slot, _) in batch_rows {
                                match page.set_xmax(slot, xmin) {
                                    Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                                    Err(e) => return Err(e),
                                }
                            }
                            page.recompute_crc();
                            pool.write_page(&page)?;
                            pool.unpin(r.page_id);
                            stats.records_undone += 1;
                        }
                    }
                }
            }
        }
    }

    // Flush all recovered pages to disk.
    pool.flush_all(INVALID_LSN)?;

    tracing::info!(
        redone = stats.records_redone,
        undone = stats.records_undone,
        incomplete_txns = stats.incomplete_txns,
        incomplete_user_txns = stats.incomplete_user_txns,
        "recovery: complete"
    );

    Ok((control, stats))
}

fn redo_record(r: &WalRecord, pool: &BufferPool, page_size: usize) -> Result<()> {
    match r.rec_type {
        WAL_FPI => {
            // P1.a torn-page protection. The redo payload is the entire clean
            // page image captured before the first modification of this page in
            // the checkpoint interval. Overwrite whatever is on disk (which may
            // be torn — half-old/half-new from an interrupted 8 KiB write),
            // establishing the clean base; the interval's subsequent
            // incremental redo records for this page (higher LSN, appearing
            // later in this pass) replay on top. Unconditional and idempotent:
            // re-running recovery re-writes the same base and re-derives the
            // same final page. The image bytes carry their own (pre-change) LSN,
            // which is below every following record's LSN, so the LSN-gated
            // incremental redos below all still apply.
            // (`restore_page_image` writes straight to the mmap; it pins no
            // frame, so there is nothing to unpin here.)
            pool.restore_page_image(r.page_id, &r.redo)?;
        }
        WAL_INDEX => {
            // P3.a durable B-Tree. The redo payload is a full node/meta page
            // image; overwrite the on-disk page with it, stamped with this
            // record's LSN, exactly like a WAL_FPI base image. Unconditional and
            // idempotent — a later WAL_INDEX for the same page (higher LSN,
            // appearing later in this LSN-ordered pass) overwrites again, so the
            // last committed image wins. Index pages never overlap heap pages,
            // so no LSN gate against incremental heap redos is needed.
            // (`restore_page_image` writes straight to the mmap and ensures the
            // file is sized; it pins no frame, so nothing to unpin.)
            let mut img = SlottedPage::from_bytes_unchecked(r.redo.clone());
            img.set_lsn(r.lsn);
            pool.restore_page_image(r.page_id, img.as_bytes())?;
        }
        WAL_INSERT => {
            if r.slot == u16::MAX {
                // Page-allocation sentinel (alloc_heap_page).  Item 82: we no
                // longer write a WAL_FPI before the first Phase-B
                // WAL_INSERT_BATCH for this page (heap.rs marks it fpi_logged
                // on allocation).  Recovery must therefore initialise the page
                // to its exact blank state here — so that a torn write of the
                // subsequent Phase-B fill is caught by the CRC check inside
                // fetch_or_create and a clean base is re-established.
                //
                // Implementation: `write_page` goes straight to mmap and is
                // pin-free (it updates the frame's dirty flag if the frame
                // happens to be in the pool, but does NOT pin/unpin).  We
                // must NOT go through fetch_or_create (which would pin) and
                // return without unpinning — see the original comment.
                pool.ensure_page_allocated(r.page_id)?;
                // Only re-initialise if the page's LSN is below alloc_lsn; if
                // page.lsn() > r.lsn the page was already filled with rows by
                // a later, fully-applied redo record — leave it alone.
                let existing_lsn = {
                    // read_page does not pin; it re-reads from mmap.
                    match pool.read_page(r.page_id) {
                        Ok(p) => p.lsn(),
                        Err(_) => 0, // page not yet in pool — treat as uninitialised
                    }
                };
                if existing_lsn < r.lsn {
                    let mut blank = SlottedPage::new(r.page_id, PAGE_TYPE_HEAP, page_size);
                    blank.set_lsn(r.lsn);
                    pool.write_page(&blank)?;
                }
                return Ok(());
            }
            let mut page = fetch_or_create(pool, r.page_id, page_size)?;
            // Only redo if current slot count ≤ slot (idempotent redo). Unpin on
            // this early return too — the same pin-leak hazard as above.
            if r.slot < page.slot_count_pub() {
                pool.unpin(r.page_id);
                return Ok(()); // already applied
            }
            // M1: redo payload is [xmin:8][prev_page:4][prev_slot:2][payload]
            // (heap.rs::encode_insert_redo), not bare payload bytes.
            let (xmin, prev, payload) = decode_insert_redo(&r.redo)?;
            page.insert_versioned(payload, xmin, 0, prev)?;
            page.set_lsn(r.lsn);
            pool.write_page(&page)?;
            pool.unpin(r.page_id);
        }
        WAL_UPDATE => {
            let mut page = fetch_or_create(pool, r.page_id, page_size)?;
            if page.lsn() >= r.lsn {
                pool.unpin(r.page_id);
                return Ok(()); // already at or past this LSN
            }
            // M1: WAL_UPDATE is now only ever an xmax stamp (DELETE, or an
            // UPDATE's old-version half) — the redo payload IS the new xmax
            // value (8 bytes), not a full replacement payload.
            let xmax = decode_xmax(&r.redo)?;
            page.set_xmax(r.slot, xmax)?;
            page.set_lsn(r.lsn);
            pool.write_page(&page)?;
            pool.unpin(r.page_id);
        }
        WAL_DELETE => {
            let mut page = fetch_or_create(pool, r.page_id, page_size)?;
            if page.lsn() >= r.lsn {
                pool.unpin(r.page_id);
                return Ok(());
            }
            page.delete(r.slot)?;
            page.set_lsn(r.lsn);
            pool.write_page(&page)?;
            pool.unpin(r.page_id);
        }
        WAL_VACUUM => {
            // M10, redo-only + idempotent. Two shapes, distinguished by slot:
            //   slot == u16::MAX : redo payload is a full compacted page image
            //     (M10.d) — reconstruct it and re-stamp this record's LSN.
            //   otherwise        : mark that one line pointer DEAD (M10.b).
            // The page-LSN check makes both a no-op once already applied (e.g.
            // a later reuse of the slot bumped the page past this record).
            let mut page = fetch_or_create(pool, r.page_id, page_size)?;
            if page.lsn() >= r.lsn {
                pool.unpin(r.page_id);
                return Ok(());
            }
            if r.slot == u16::MAX {
                let mut img = SlottedPage::from_bytes_unchecked(r.redo.clone());
                img.set_lsn(r.lsn);
                pool.write_page(&img)?;
            } else {
                page.mark_dead(r.slot)?;
                page.set_lsn(r.lsn);
                pool.write_page(&page)?;
            }
            pool.unpin(r.page_id);
        }
        WAL_XMAX_BATCH => {
            // Redo: stamp xmax = xid on every listed slot. LSN-gated so
            // re-running recovery is idempotent.
            let (xid, slots) = decode_xmax_batch_redo(&r.redo)?;
            let mut page = fetch_or_create(pool, r.page_id, page_size)?;
            if page.lsn() >= r.lsn {
                pool.unpin(r.page_id);
                return Ok(());
            }
            for slot in slots {
                match page.set_xmax(slot, xid) {
                    Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                    Err(e) => return Err(e),
                }
            }
            page.set_lsn(r.lsn);
            pool.write_page(&page)?;
            pool.unpin(r.page_id);
        }
        WAL_HOT_XPAGE_HEAD => {
            // item 71: cross-page HOT old-page head record. Redo:
            //   (a) stamp xmax = xid on old_slot.
            //   (b) set hot_next = HOT_NEXT_XPAGE + overwrite prev_page/prev_slot
            //       with (new_page_id, new_slot) on old_slot.
            //
            // redo payload: xid (8 B) || old_slot (2 B) || new_page_id (4 B) || new_slot (2 B)
            // LSN-gated (same pattern as WAL_HOT_UPDATE).
            if r.redo.len() < 16 {
                // Length check before fetch_or_create so we don't leak a pin.
                return Err(DbError::WalCorrupt { lsn: r.lsn });
            }
            let xid = u64_from_le(r.redo[0..8].try_into().unwrap());
            let old_slot = u16_from_le(r.redo[8..10].try_into().unwrap());
            let new_page_id = u32_from_le(r.redo[10..14].try_into().unwrap());
            let new_slot = u16_from_le(r.redo[14..16].try_into().unwrap());
            let mut page = fetch_or_create(pool, r.page_id, page_size)?;
            if page.lsn() >= r.lsn {
                pool.unpin(r.page_id);
                return Ok(());
            }
            // (a) xmax stamp
            match page.set_xmax(old_slot, xid) {
                Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                Err(e) => return Err(e),
            }
            // (b) cross-page forwarding pointer
            match page.set_hot_xpage(old_slot, new_page_id, new_slot) {
                Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                Err(e) => return Err(e),
            }
            page.set_lsn(r.lsn);
            pool.write_page(&page)?;
            pool.unpin(r.page_id);
            // WAL_INSERT redo for new_page_id is handled separately (higher LSN).
        }
        WAL_HOT_UPDATE => {
            // item 58: atomic same-page HOT update. One WAL record covers:
            //   (a) xmax stamp on old slot, (b) hot_next pointer in old slot,
            //   (c) new-version insert at new slot — all on the same page.
            //
            // redo payload: xid (8 B) || old_slot (2 B) || new_slot (2 B)
            //               || insert_redo (variable, same layout as WAL_INSERT redo)
            //
            // LSN-gated: skip if page.lsn() >= record.lsn.
            let (xid, old_slot, new_slot, insert_redo) = decode_hot_update_redo(&r.redo)?;
            let mut page = fetch_or_create(pool, r.page_id, page_size)?;
            if page.lsn() >= r.lsn {
                pool.unpin(r.page_id);
                return Ok(());
            }
            // (a) xmax-stamp the old slot.
            match page.set_xmax(old_slot, xid) {
                Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                Err(e) => return Err(e),
            }
            // (b) set hot_next forwarding pointer on old slot.
            match page.set_hot_next(old_slot, new_slot) {
                Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                Err(e) => return Err(e),
            }
            // (c) insert the new version at new_slot (idempotent: slot-count guard).
            if new_slot >= page.slot_count_pub() {
                let (xmin, prev, payload) = decode_insert_redo(insert_redo)?;
                page.insert_versioned(payload, xmin, 0, prev)?;
            }
            page.set_lsn(r.lsn);
            pool.write_page(&page)?;
            pool.unpin(r.page_id);
        }
        WAL_INDEX_INSERT => {
            // item 56 Step 4: logical B-tree leaf insert, redo-only, LSN-gated.
            // A WAL_FPI for this leaf preceded this record (lower LSN, already
            // replayed) restoring the clean pre-insert base; this record
            // re-executes the entry insert. `r.slot` = insertion position.
            //
            // redo payload: key_len (2 B LE) || key_bytes || rid_page (4 B LE) || rid_slot (2 B LE)
            let page = match pool.fetch_page(r.page_id) {
                Ok(p) => p,
                Err(DbError::PageNotFound { .. }) => {
                    // FPI should have sized the file; skip gracefully.
                    tracing::warn!(
                        lsn = r.lsn,
                        page_id = r.page_id,
                        "WAL_INDEX_INSERT redo: index leaf page not found, skipping"
                    );
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            if page.lsn() >= r.lsn {
                pool.unpin(r.page_id);
                return Ok(());
            }
            if r.redo.len() < 2 {
                pool.unpin(r.page_id);
                return Err(DbError::WalCorrupt { lsn: r.lsn });
            }
            let key_len = u16_from_le(r.redo[0..2].try_into().unwrap()) as usize;
            if r.redo.len() < 2 + key_len + 6 {
                pool.unpin(r.page_id);
                return Err(DbError::WalCorrupt { lsn: r.lsn });
            }
            let key_bytes = &r.redo[2..2 + key_len];
            let rid_page = u32_from_le(r.redo[2 + key_len..6 + key_len].try_into().unwrap());
            let rid_slot = u16_from_le(r.redo[6 + key_len..8 + key_len].try_into().unwrap());
            let rid = RowId {
                page_id: rid_page,
                slot: rid_slot,
            };
            match redo_index_insert(&page, r.slot, key_bytes, rid, page_size) {
                Ok(mut new_page) => {
                    new_page.set_lsn(r.lsn);
                    pool.write_page(&new_page)?;
                }
                Err(e) => {
                    tracing::warn!(lsn = r.lsn, error = %e, "WAL_INDEX_INSERT redo skipped");
                }
            }
            pool.unpin(r.page_id);
        }
        WAL_INSERT_BATCH => {
            // Item 79: redo N inserts on the same fill page. LSN-gated.
            // Per-row redo data = encode_insert_redo layout (xmin 8B + prev_page 4B +
            // prev_slot 2B + payload); reuses decode_insert_redo for each row.
            // Idempotent: skip rows whose slot already exists (slot < slot_count_pub).
            let batch_rows = decode_insert_batch_redo(&r.redo)?;
            let mut page = fetch_or_create(pool, r.page_id, page_size)?;
            if page.lsn() >= r.lsn {
                pool.unpin(r.page_id);
                return Ok(());
            }
            for (slot, row_redo) in &batch_rows {
                if *slot >= page.slot_count_pub() {
                    let (row_xmin, prev, payload) = decode_insert_redo(row_redo)?;
                    page.insert_versioned(payload, row_xmin, 0, prev)?;
                }
            }
            page.set_lsn(r.lsn);
            pool.write_page(&page)?;
            pool.unpin(r.page_id);
        }
        WAL_HOT_XPAGE_BATCH => {
            // Item 80: redo N (old_slot, new_page_id, new_slot) entries on the same
            // old page. LSN-gated; idempotent via set_xmax/set_hot_xpage.
            let (xid, entries) = decode_hot_xpage_batch_redo(&r.redo)?;
            let mut page = fetch_or_create(pool, r.page_id, page_size)?;
            if page.lsn() >= r.lsn {
                pool.unpin(r.page_id);
                return Ok(());
            }
            for &(old_slot, new_page_id, new_slot) in &entries {
                match page.set_xmax(old_slot, xid) {
                    Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                    Err(e) => return Err(e),
                }
                match page.set_hot_xpage(old_slot, new_page_id, new_slot) {
                    Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                    Err(e) => return Err(e),
                }
            }
            page.set_lsn(r.lsn);
            pool.write_page(&page)?;
            pool.unpin(r.page_id);
        }
        _ => {}
    }
    Ok(())
}

fn undo_record(r: &WalRecord, pool: &BufferPool, page_size: usize) -> Result<()> {
    match r.rec_type {
        WAL_INSERT => {
            // Undo an insert = delete the slot.
            if r.slot == u16::MAX {
                return Ok(());
            }
            let mut page = fetch_or_create(pool, r.page_id, page_size)?;
            match page.delete(r.slot) {
                Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                Err(e) => return Err(e),
            }
            pool.write_page(&page)?;
            pool.unpin(r.page_id);
        }
        WAL_UPDATE => {
            // Undo an xmax stamp = restore the old xmax (stored in undo payload).
            let mut page = fetch_or_create(pool, r.page_id, page_size)?;
            let old_xmax = decode_xmax(&r.undo)?;
            match page.set_xmax(r.slot, old_xmax) {
                Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                Err(e) => return Err(e),
            }
            page.recompute_crc();
            pool.write_page(&page)?;
            pool.unpin(r.page_id);
        }
        WAL_XMAX_BATCH => {
            // Undo a batch xmax stamp = reset all listed slots' xmax to 0.
            // Old xmax was provably 0 (conflict check enforces this before any
            // stamp); undo payload carries the slot list to iterate.
            let slots = decode_xmax_batch_undo(&r.undo)?;
            let mut page = fetch_or_create(pool, r.page_id, page_size)?;
            for slot in slots {
                match page.set_xmax(slot, 0) {
                    Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                    Err(e) => return Err(e),
                }
            }
            page.recompute_crc();
            pool.write_page(&page)?;
            pool.unpin(r.page_id);
        }
        WAL_DELETE => {
            // Undo a delete = re-insert the old tuple at same slot position.
            // Simple approach: insert anew (slot may differ, but for M0 this is fine).
            let mut page = fetch_or_create(pool, r.page_id, page_size)?;
            page.insert(&r.undo)?;
            pool.write_page(&page)?;
            pool.unpin(r.page_id);
        }
        WAL_HOT_XPAGE_HEAD => {
            // Undo the old-page portion of a cross-page HOT update (item 71).
            // undo payload: old_slot (2 B LE) || saved_prev_page (4 B LE) || saved_prev_slot (2 B LE)
            //
            // The new version (on new_page_id, logged via WAL_INSERT) is handled
            // by the WAL_INSERT undo arm in the same pass (mark_dead on that slot).
            if r.undo.len() < 8 {
                // Length check before fetch_or_create so we don't leak a pin.
                return Err(DbError::WalCorrupt { lsn: r.lsn });
            }
            let old_slot = u16_from_le(r.undo[0..2].try_into().unwrap());
            let saved_prev_page = u32_from_le(r.undo[2..6].try_into().unwrap());
            let saved_prev_slot = u16_from_le(r.undo[6..8].try_into().unwrap());
            let mut page = fetch_or_create(pool, r.page_id, page_size)?;
            // Restore prev_page/prev_slot + clear hot_next (idempotent).
            match page.restore_prev_and_hot_next(old_slot, saved_prev_page, saved_prev_slot) {
                Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                Err(e) => return Err(e),
            }
            // Clear xmax (idempotent — TupleDeleted means slot already dead).
            match page.set_xmax(old_slot, 0) {
                Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                Err(e) => return Err(e),
            }
            page.recompute_crc();
            pool.write_page(&page)?;
            pool.unpin(r.page_id);
        }
        WAL_INSERT_BATCH => {
            // Undo N inserts on the same fill page: delete every slot in the batch.
            // Undo payload: n_slots (2 B LE) || slot_0 (2 B LE) || ...
            let slots = decode_insert_batch_undo(&r.undo)?;
            let mut page = fetch_or_create(pool, r.page_id, page_size)?;
            for slot in slots {
                match page.delete(slot) {
                    Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                    Err(e) => return Err(e),
                }
            }
            pool.write_page(&page)?;
            pool.unpin(r.page_id);
        }
        WAL_HOT_XPAGE_BATCH => {
            // Undo N old-page HOT_XPAGE_HEAD stamps on the same old page.
            // Undo payload: n_entries (2 B LE) || for each: old_slot (2 B LE) +
            //   saved_prev_page (4 B LE) + saved_prev_slot (2 B LE).
            // For each entry: restore_prev_and_hot_next + clear xmax = 0.
            let entries = decode_hot_xpage_batch_undo_entries(&r.undo)?;
            let mut page = fetch_or_create(pool, r.page_id, page_size)?;
            for (old_slot, saved_prev_page, saved_prev_slot) in entries {
                match page.restore_prev_and_hot_next(old_slot, saved_prev_page, saved_prev_slot) {
                    Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                    Err(e) => return Err(e),
                }
                match page.set_xmax(old_slot, 0) {
                    Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                    Err(e) => return Err(e),
                }
            }
            page.recompute_crc();
            pool.write_page(&page)?;
            pool.unpin(r.page_id);
        }
        WAL_HOT_UPDATE => {
            // Undo an HOT update: two-phase, order-sensitive (see design note in
            // format.rs WAL_HOT_UPDATE comment and crash test P59b).
            //
            // undo payload: old_slot (2 B LE) || new_slot (2 B LE)
            //
            // Phase 1 (new-slot first): delete the new-version slot — marks it
            //   Unused so the B-tree candidate (which points to old_slot) will
            //   see the old slot correctly after phase 2.
            // Phase 2 (old-slot last): clear hot_next (set to HOT_NEXT_NONE),
            //   then clear xmax = 0 — restoring the old slot to live.
            //
            // The ordering is critical: if we crash between phase 1 and phase 2,
            // the old slot has xmax set but no hot_next pointer. Crash-test P59b
            // verifies that undo is idempotent when re-run after such a crash:
            // on re-open, recovery re-runs undo for the incomplete HOT mini-txn,
            // which finds the new slot already deleted (idempotent delete) and
            // completes the old-slot restore.
            let (old_slot, new_slot) = decode_hot_update_undo(&r.undo)?;
            let mut page = fetch_or_create(pool, r.page_id, page_size)?;
            // Phase 1: delete new slot (idempotent — TupleDeleted is OK).
            match page.delete(new_slot) {
                Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                Err(e) => return Err(e),
            }
            // Phase 2a: clear hot_next on old slot (idempotent).
            match page.set_hot_next(old_slot, HOT_NEXT_NONE) {
                Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                Err(e) => return Err(e),
            }
            // Phase 2b: clear xmax on old slot (idempotent).
            match page.set_xmax(old_slot, 0) {
                Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                Err(e) => return Err(e),
            }
            page.recompute_crc();
            pool.write_page(&page)?;
            pool.unpin(r.page_id);
        }
        _ => {}
    }
    Ok(())
}

/// Decode an xmax-stamp WAL redo/undo payload (8 bytes LE): the value *is*
/// the xmax to apply, since a stamp's payload is nothing but the new xmax.
fn decode_xmax(buf: &[u8]) -> Result<u64> {
    let arr: [u8; 8] = buf.try_into().map_err(|_| DbError::WalCorrupt { lsn: 0 })?;
    Ok(u64_from_le(arr))
}

/// Decode a WAL_XMAX_BATCH redo payload:
///   `xid (8 B LE) || n_slots (2 B LE) || slot_0 (2 B LE) || ...`
fn decode_xmax_batch_redo(buf: &[u8]) -> Result<(u64, Vec<u16>)> {
    if buf.len() < 10 {
        return Err(DbError::WalCorrupt { lsn: 0 });
    }
    let xid = u64_from_le(buf[0..8].try_into().unwrap());
    let n = u16_from_le(buf[8..10].try_into().unwrap()) as usize;
    if buf.len() < 10 + 2 * n {
        return Err(DbError::WalCorrupt { lsn: 0 });
    }
    let slots = (0..n)
        .map(|i| u16_from_le(buf[10 + 2 * i..12 + 2 * i].try_into().unwrap()))
        .collect();
    Ok((xid, slots))
}

/// Decode a WAL_XMAX_BATCH undo payload:
///   `n_slots (2 B LE) || slot_0 (2 B LE) || ...`
fn decode_xmax_batch_undo(buf: &[u8]) -> Result<Vec<u16>> {
    if buf.len() < 2 {
        return Err(DbError::WalCorrupt { lsn: 0 });
    }
    let n = u16_from_le(buf[0..2].try_into().unwrap()) as usize;
    if buf.len() < 2 + 2 * n {
        return Err(DbError::WalCorrupt { lsn: 0 });
    }
    let slots = (0..n)
        .map(|i| u16_from_le(buf[2 + 2 * i..4 + 2 * i].try_into().unwrap()))
        .collect();
    Ok(slots)
}

/// Decode a WAL_HOT_UPDATE redo payload:
///   `xid (8 B LE) || old_slot (2 B LE) || new_slot (2 B LE) || insert_redo (variable)`
/// Returns `(xid, old_slot, new_slot, insert_redo_slice)`.
fn decode_hot_update_redo(buf: &[u8]) -> Result<(u64, u16, u16, &[u8])> {
    if buf.len() < 12 {
        return Err(DbError::WalCorrupt { lsn: 0 });
    }
    let xid = u64_from_le(buf[0..8].try_into().unwrap());
    let old_slot = u16_from_le(buf[8..10].try_into().unwrap());
    let new_slot = u16_from_le(buf[10..12].try_into().unwrap());
    let insert_redo = &buf[12..];
    Ok((xid, old_slot, new_slot, insert_redo))
}

/// Decode a WAL_HOT_UPDATE undo payload:
///   `old_slot (2 B LE) || new_slot (2 B LE)`
fn decode_hot_update_undo(buf: &[u8]) -> Result<(u16, u16)> {
    if buf.len() < 4 {
        return Err(DbError::WalCorrupt { lsn: 0 });
    }
    let old_slot = u16_from_le(buf[0..2].try_into().unwrap());
    let new_slot = u16_from_le(buf[2..4].try_into().unwrap());
    Ok((old_slot, new_slot))
}

/// Decode a WAL_INSERT_BATCH redo payload, returning per-row `(slot, row_redo_bytes)`.
///
/// Redo format: `xmin (8 B LE) || n_rows (2 B LE) || for each: slot (2 B LE) + redo_len (4 B LE) + redo_data`
///
/// `redo_data` per row = `encode_insert_redo` layout (xmin 8B + prev_page 4B + prev_slot 2B + payload).
/// Decoded further by `decode_insert_redo` in the redo handler.
fn decode_insert_batch_redo(buf: &[u8]) -> Result<Vec<(u16, Vec<u8>)>> {
    if buf.len() < 10 {
        return Err(DbError::WalCorrupt { lsn: 0 });
    }
    let n = u16_from_le(buf[8..10].try_into().unwrap()) as usize;
    let mut rows = Vec::with_capacity(n);
    let mut pos = 10;
    for _ in 0..n {
        if pos + 6 > buf.len() {
            return Err(DbError::WalCorrupt { lsn: 0 });
        }
        let slot = u16_from_le(buf[pos..pos + 2].try_into().unwrap());
        let redo_len = u32_from_le(buf[pos + 2..pos + 6].try_into().unwrap()) as usize;
        pos += 6;
        if pos + redo_len > buf.len() {
            return Err(DbError::WalCorrupt { lsn: 0 });
        }
        rows.push((slot, buf[pos..pos + redo_len].to_vec()));
        pos += redo_len;
    }
    Ok(rows)
}

/// Decode a WAL_INSERT_BATCH undo payload: slot list for deletion.
///
/// Undo format: `n_slots (2 B LE) || slot_0 (2 B LE) || ...`
fn decode_insert_batch_undo(buf: &[u8]) -> Result<Vec<u16>> {
    if buf.len() < 2 {
        return Err(DbError::WalCorrupt { lsn: 0 });
    }
    let n = u16_from_le(buf[0..2].try_into().unwrap()) as usize;
    if buf.len() < 2 + 2 * n {
        return Err(DbError::WalCorrupt { lsn: 0 });
    }
    Ok((0..n)
        .map(|i| u16_from_le(buf[2 + 2 * i..4 + 2 * i].try_into().unwrap()))
        .collect())
}

/// `(old_slot, new_page_id, new_slot)` tuples decoded from a WAL_HOT_XPAGE_BATCH record.
type HotXpageBatchEntries = Vec<(u16, u32, u16)>;

/// Decode a WAL_HOT_XPAGE_BATCH redo payload.
///
/// Redo format: `xid (8 B LE) || n_entries (2 B LE) || for each: old_slot (2 B LE) + new_page_id (4 B LE) + new_slot (2 B LE)`
fn decode_hot_xpage_batch_redo(buf: &[u8]) -> Result<(Xid, HotXpageBatchEntries)> {
    if buf.len() < 10 {
        return Err(DbError::WalCorrupt { lsn: 0 });
    }
    let xid = u64_from_le(buf[0..8].try_into().unwrap());
    let n = u16_from_le(buf[8..10].try_into().unwrap()) as usize;
    if buf.len() < 10 + 8 * n {
        return Err(DbError::WalCorrupt { lsn: 0 });
    }
    let entries = (0..n)
        .map(|i| {
            let base = 10 + 8 * i;
            let old_slot = u16_from_le(buf[base..base + 2].try_into().unwrap());
            let new_page_id = u32_from_le(buf[base + 2..base + 6].try_into().unwrap());
            let new_slot = u16_from_le(buf[base + 6..base + 8].try_into().unwrap());
            (old_slot, new_page_id, new_slot)
        })
        .collect();
    Ok((xid, entries))
}

/// Decode a WAL_HOT_XPAGE_BATCH undo payload.
///
/// Undo format: `n_entries (2 B LE) || for each: old_slot (2 B LE) + saved_prev_page (4 B LE) + saved_prev_slot (2 B LE)`
fn decode_hot_xpage_batch_undo_entries(buf: &[u8]) -> Result<Vec<(u16, u32, u16)>> {
    if buf.len() < 2 {
        return Err(DbError::WalCorrupt { lsn: 0 });
    }
    let n = u16_from_le(buf[0..2].try_into().unwrap()) as usize;
    if buf.len() < 2 + 8 * n {
        return Err(DbError::WalCorrupt { lsn: 0 });
    }
    Ok((0..n)
        .map(|i| {
            let base = 2 + 8 * i;
            let old_slot = u16_from_le(buf[base..base + 2].try_into().unwrap());
            let saved_prev_page = u32_from_le(buf[base + 2..base + 6].try_into().unwrap());
            let saved_prev_slot = u16_from_le(buf[base + 6..base + 8].try_into().unwrap());
            (old_slot, saved_prev_page, saved_prev_slot)
        })
        .collect())
}

fn fetch_or_create(pool: &BufferPool, page_id: u32, page_size: usize) -> Result<SlottedPage> {
    match pool.fetch_page(page_id) {
        Ok(p) => {
            // Item 82: CRC-based torn-page detection.  If the on-disk image has
            // an invalid checksum the page was partially written at crash time
            // (torn write).  Returning a blank page lets every redo handler
            // re-apply its records from first principles rather than relying on
            // a corrupted LSN field (which may falsely appear ≥ the record's
            // LSN, causing redo to be incorrectly skipped).
            //
            // Note: `pool.fetch_page` calls `read_page_locked`, which already
            // returns `from_bytes_unchecked` for all-zero pages (valid CRC by
            // construction) and `from_bytes` (with CRC check) for non-zero
            // pages.  A `ChecksumMismatch` propagates here as `Ok(torn_page)`
            // only when the caller was `fetch_page` directly; the bufferpool's
            // internal `read_page_from_mmap` re-uses the same path.  For
            // safety we verify again here for any page that may have been
            // partially written.
            if p.verify_crc().is_err() {
                // Torn page: decrement pin and return blank so redo rebuilds.
                pool.unpin(page_id);
                Ok(SlottedPage::new(page_id, PAGE_TYPE_HEAP, page_size))
            } else {
                Ok(p)
            }
        }
        Err(DbError::PageNotFound { .. }) => {
            // Grow the file to include this page when replaying into a
            // smaller-than-implied data file (e.g. a replica/restore applying WAL
            // onto a page beyond its base, P6.c/P6.d) — normal crash recovery,
            // where the file is already sized, leaves this a no-op.
            pool.ensure_page_allocated(page_id)?;
            Ok(SlottedPage::new(page_id, PAGE_TYPE_HEAP, page_size))
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bufferpool::BufferPool;
    use crate::control;
    use crate::format::{DEFAULT_PAGE_SIZE, INVALID_LSN};
    use crate::heap::Heap;
    use crate::wal::Wal;
    use tempfile::tempdir;

    fn paths(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        (dir.join("control"), dir.join("data.db"), dir.join("db.wal"))
    }

    #[test]
    fn clean_recovery_no_incomplete() {
        let dir = tempdir().unwrap();
        let (ctrl_p, data_p, wal_p) = paths(dir.path());

        control::create(&ctrl_p, DEFAULT_PAGE_SIZE).unwrap();
        let pool = BufferPool::open(&data_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        let wal = Wal::open(&wal_p, INVALID_LSN).unwrap();
        let heap = Heap::new(DEFAULT_PAGE_SIZE as usize);

        let rid = heap.insert(b"persistent", 1, &pool, &wal).unwrap();
        pool.flush_all(wal.durable_lsn()).unwrap();
        drop(pool);
        drop(wal);

        let (_, stats) = recover(&ctrl_p, &data_p, &wal_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        assert_eq!(stats.incomplete_txns, 0);
        assert_eq!(stats.records_undone, 0);
        let _ = rid;
    }

    #[test]
    fn incomplete_user_txn_detected_and_undone() {
        let dir = tempdir().unwrap();
        let (ctrl_p, data_p, wal_p) = paths(dir.path());
        control::create(&ctrl_p, DEFAULT_PAGE_SIZE).unwrap();

        let rid = {
            let pool = BufferPool::open(&data_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
            let wal = Wal::open(&wal_p, INVALID_LSN).unwrap();
            let heap = Heap::new(DEFAULT_PAGE_SIZE as usize);
            let xid = 7;
            wal.begin_user_txn(xid).unwrap();
            let rid = heap.insert(b"never_committed", xid, &pool, &wal).unwrap();
            // No WAL_TXN_COMMIT — simulates a crash mid-user-transaction.
            // The statement's own mini-txn is already durably committed
            // (D2), but the user transaction as a whole never finished.
            pool.flush_all(wal.durable_lsn()).unwrap();
            drop(pool);
            drop(wal);
            rid
        };

        let (_, stats) = recover(&ctrl_p, &data_p, &wal_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        assert_eq!(
            stats.incomplete_user_txns, 1,
            "must detect the incomplete user txn"
        );
        assert!(stats.records_undone > 0, "must undo the orphaned insert");

        // After recovery, the row must be permanently invisible.
        let pool = BufferPool::open(&data_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        let heap = Heap::new(DEFAULT_PAGE_SIZE as usize);
        let snap = crate::mvcc::Snapshot::new(100, 100, vec![]);
        assert!(heap.get(rid, &snap, 100, &pool).is_err());
    }

    #[test]
    fn committed_user_txn_is_not_undone() {
        let dir = tempdir().unwrap();
        let (ctrl_p, data_p, wal_p) = paths(dir.path());
        control::create(&ctrl_p, DEFAULT_PAGE_SIZE).unwrap();

        let rid = {
            let pool = BufferPool::open(&data_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
            let wal = Wal::open(&wal_p, INVALID_LSN).unwrap();
            let heap = Heap::new(DEFAULT_PAGE_SIZE as usize);
            let xid = 7;
            let begin_lsn = wal.begin_user_txn(xid).unwrap();
            let rid = heap.insert(b"survives", xid, &pool, &wal).unwrap();
            wal.commit_user_txn(xid, begin_lsn).unwrap();
            pool.flush_all(wal.durable_lsn()).unwrap();
            drop(pool);
            drop(wal);
            rid
        };

        let (_, stats) = recover(&ctrl_p, &data_p, &wal_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        assert_eq!(stats.incomplete_user_txns, 0);

        let pool = BufferPool::open(&data_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        let heap = Heap::new(DEFAULT_PAGE_SIZE as usize);
        let snap = crate::mvcc::Snapshot::new(100, 100, vec![]);
        assert_eq!(heap.get(rid, &snap, 100, &pool).unwrap(), b"survives");
    }

    #[test]
    fn recovery_redoes_committed_insert() {
        let dir = tempdir().unwrap();
        let (ctrl_p, data_p, wal_p) = paths(dir.path());

        control::create(&ctrl_p, DEFAULT_PAGE_SIZE).unwrap();
        {
            let pool = BufferPool::open(&data_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
            let wal = Wal::open(&wal_p, INVALID_LSN).unwrap();
            let heap = Heap::new(DEFAULT_PAGE_SIZE as usize);
            heap.insert(b"survived", 1, &pool, &wal).unwrap();
            // Simulate crash: do NOT flush page to disk.
            drop(wal);
            drop(pool);
        }

        // Recovery should redo the committed insert.
        let (_, stats) = recover(&ctrl_p, &data_p, &wal_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        assert!(stats.records_redone > 0);
    }
}
