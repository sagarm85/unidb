# Unconditional/bulk DELETE pays one WAL mini-transaction per row

**Type:** Performance
**Status:** SHIPPED — `Heap::delete_many` landed in PR #TBD (`47-44-perf-batch`, 2026-07-16). WAL B/row: 230 → 107 (−53%); throughput 416k rec/s at 5000 rows. See `PROGRESS.md` "Items 47 + 44" entry for full metrics and invariant analysis.

---

## Problem

`DELETE FROM t` (no predicate) measured **postgres +275%** in Table 3's CRUD
stress at 20,000 rows (`docs/performance/multi_model_report_20260715_092725.md`):
unidb 271,703 rec/s vs Postgres 1,122,804 rec/s. `DELETE selected (k>=N)`
(a predicated bulk delete) shows the same shape, +409%.

This is a **different root cause from item 43** (the scan-vs-index gate) —
`DELETE FROM t` has no predicate at all, so there is no scan-vs-index decision
to make; the gap is in the deletion mechanics themselves once the matching
rows are known.

## Root cause

`exec_delete` (`src/sql/executor.rs:2207`) calls `heap.delete(row_id, ...)`
**once per matched row**, in a loop. `Heap::delete` (`src/heap.rs:399`) is a
**fully self-contained mini-transaction per call**:

```rust
pub fn delete(&self, row_id: RowId, xid: Xid, pool: &BufferPool, wal: &Wal,
              lock_mgr: &LockManager) -> Result<()> {
    lock_mgr.try_acquire_write(...)?;
    let (txn_id, begin_lsn) = wal.begin_mini_txn()?;      // ← per-row
    let _wg = pool.latch_exclusive(row_id.page_id);        // ← per-row
    let mut page = pool.fetch_page_for_write(...)?;
    ...
    let prev_lsn = pool.maybe_log_fpi(...)?...;            // ← per-row FPI check
    let lsn = wal.log_update(...)?;                         // ← per-row WAL record
    ...
    wal.commit_mini_txn(txn_id, lsn)?;                      // ← per-row
    Ok(())
}
```

A DELETE touching N rows performs **N separate WAL mini-transactions**, N
separate exclusive page-latch acquisitions, and N separate full-page-image
checks — this is the *exact same shape* of inefficiency item 40 already found
and fixed for `CREATE INDEX` (one `tree.insert()` mini-txn per row instead of
one bulk mini-txn for the whole build) and item 35 found for `enforce_unique`.
`send_event_capture` also runs per row in this loop, though it's a cheap
early-return when the table has no events enabled (confirmed: not a factor in
the specific benchmark that surfaced this, `benches/decompose.rs`'s Table 3
CRUD table never calls `enable_events`) — it would become a real cost on a
table that *does* have CDC enabled, worth re-checking once the main fix lands.

## Why this is a natural, lower-risk extension (not a new architecture)

`matching_rows` (`src/sql/executor.rs:2323`) — the function that resolves
which rows a DELETE/UPDATE will touch — **already sorts candidates into
physical `(page_id, slot)` order** (the "B5" comment, added to make `heap.get`
walk sequentially rather than randomly during the *read* side). That means
the rows a bulk DELETE will touch are already grouped by page by the time
`exec_delete`'s loop sees them — the natural fix is to **batch the deletes by
page**: one mini-transaction (one WAL begin/commit, one full-page-image check)
per *page*, applying every matched row's `xmax` stamp on that page within it,
instead of one mini-transaction per *row*. For a table where deleted rows
cluster into pages (the common case), this turns thousands of mini-txns into
however many distinct pages were touched — the same `N → num_pages` reduction
item 40 already proved for `CREATE INDEX` (134.2s → 12.0s, 11.2×).

## Proposed scope (re-derive the exact mechanism per CLAUDE.md §0.6.2 before
implementing — this is a sketch, not a spec to implement as-is)

1. **A batched delete entry point** — e.g. `Heap::delete_many(row_ids: &[RowId],
   xid, pool, wal, lock_mgr)` — groups the (already page-sorted) `row_ids` by
   `page_id`, and for each page: acquire the exclusive latch once, do the
   full-page-image check once, stamp `xmax` for every row on that page, write
   one WAL record covering the page's changes, one `commit_mini_txn`.
2. **`exec_delete` calls the batched path** when there's no per-row
   side-effect that requires row-by-row sequencing (see invariants below for
   what "no side-effect" means precisely — FK RESTRICT checks and event
   capture both currently run per-row *before* `heap.delete`, and need to stay
   correct if batching changes the order/grouping of the actual heap
   mutation).
3. **`exec_update`'s equivalent path** (if `UPDATE ... SET` also does one
   mini-txn per row today — verify this, don't assume it mirrors DELETE
   exactly) may benefit from the same pattern; scope-check before including it
   in the same change or as a fast-follow.

## Correctness invariants the fix MUST preserve

1. **WAL-before-page (D5) unaffected** — batching mini-transactions per page,
   not skipping WAL logging; every page mutation is still WAL-logged before
   the page write, just coalesced to one record per page instead of one per
   row on that page.
2. **FK RESTRICT (item 36) correctness preserved** — `enforce_fk_restrict`
   currently runs per-row, before `heap.delete`, using a fresh snapshot each
   time (`ctx.txn_mgr.snapshot_for_statement`). If deletes get grouped by
   page, the RESTRICT check for each row must still run — and must still see
   a *correct* snapshot (does grouping change what a later row's RESTRICT
   check should see about earlier rows in the same batch that are about to be
   deleted too? Re-derive this carefully — it's the same MVCC-visibility class
   of hazard items 35/36's own invariants already had to get right).
3. **Undo/rollback correctness** — `ctx.txn_mgr.record_undo` currently records
   one `UndoAction::XmaxStamp` per row; a batched delete must still produce
   correct per-row undo entries so an abort correctly reverts every stamped
   row, not just the last one touched per page.
4. **Crash-safety** — a crash mid-batched-delete must leave the table in a
   valid state (either all of a page's deletes in that mini-txn committed, or
   none — the mini-txn boundary is the atomicity unit, same as item 40's
   crash-safety analysis). New crash-harness point if the mini-txn shape
   genuinely changes; confirm, don't assume one is needed.
5. **SSI / concurrent-writer correctness (item 11/16 class)** — `ssi_note_reads`
   and `ssi_note_write` currently fire per matched row; batching must not
   silently drop or coalesce these in a way that weakens SERIALIZABLE
   conflict detection.

## Acceptance criteria

- [ ] `DELETE FROM t` (unconditional) on a large table (100k+ rows) shows a
      measured throughput improvement over today's per-row baseline —
      re-measure via `scripts/multi_model_report.sh` Table 3's "DELETE all"
      row, comparing against `multi_model_report_20260715_092725.md`'s
      271,703 rec/s baseline.
- [ ] `DELETE selected (k>=N)` (predicated bulk delete) shows a comparable
      improvement.
- [ ] WAL record count for a bulk delete drops from ≈N (one per row) to
      ≈num_pages_touched — the same style of proof item 40 recorded
      (WAL appends before/after).
- [ ] Existing FK RESTRICT correctness tests (item 36) pass unchanged — a
      still-referenced row must still block the delete, whether or not it's
      grouped into a batch with other rows.
- [ ] Existing crash-harness tests pass; a new crash point added if the
      mini-txn granularity genuinely changed (confirm first, per invariant 4).
- [ ] `cargo test --workspace` and the concurrency matrix (`benches/
      conc_matrix.rs`) pass unchanged — no new race introduced by batching.
- [ ] `PROGRESS.md` records real before/after numbers, referencing this item
      and the report that surfaced the gap.

## Depends on / builds on

- `src/sql/executor.rs` — `exec_delete`, `matching_rows` (the existing B5
  physical-order sort this fix reuses) — the code to modify.
- `src/heap.rs` — `Heap::delete`, `Heap::undo_xmax_stamp` — the mini-txn
  mechanics.
- Item 40 (`40_btree_bulk_build.md`) — the direct precedent for this exact
  class of fix (N mini-txns → num_pages mini-txns), including its
  crash-safety analysis, which this item should follow the same discipline
  as.
- Item 36 (FK row-level enforcement) — the RESTRICT-check correctness
  invariant that must survive batching.
- `docs/performance/multi_model_report_20260715_092725.md` — the report that
  surfaced this gap (Table 3, "DELETE all" and "DELETE selected" rows).
