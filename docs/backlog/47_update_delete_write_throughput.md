# UPDATE/DELETE write throughput: per-row WAL overhead, MVCC insert-new-version cost, and unchanged-index maintenance

**Type:** Performance
**Status:** PHASE A SHIPPED (PR #119, 2026-07-16) — Phase B (vectorised predicate scan on `matching_rows`) is the next open task. Phase C (HOT-equivalent chain) is deferred/milestone-sized.

### Phase A result (shipped)
WAL B/row: **619 → 465** (−25% at 500-row scale). `init_patch_batches` now covers both secondary BTree and unique-enforcement indexes; `flush_patch_batches` calls `patch_many` once per batch after the row loop. See `PROGRESS.md` "Items 47 + 44."

### Phase B — next task
**What:** Vectorised predicate scan — deform only the predicate column(s) during the `matching_rows` scan phase, not the full row. Full decode runs only on matched rows (unavoidable for producing the new version).
**Where:** `src/sql/executor.rs::matching_rows` — pass the predicate column mask (same pattern as B2 pushdown in `src/sql/query_exec.rs`); call `deform_row(page, slot, &pred_col_mask)` instead of `decode_row`.
**Signal to watch:** `cols/row` on UPDATE drops from 8.00 toward 5.00 (1 pred col × all scanned rows + 4 full cols × matched rows, normalized at 50% selectivity).
**Expected gain:** UPDATE bulk ~92k → ~150k+ rec/s; ratio vs Postgres moves from ~0.17× toward ~0.28×.
**Effort:** Low–medium. Single function change; no WAL/format/crash-harness impact.
**Priority:** High — UPDATE bulk at 0.17× PG (+481%), DELETE selected at 0.17× PG (+497%) are the largest remaining gaps. Root cause is structural: every matched row = one WAL mini-txn with full-page-image check + one B-tree update per indexed column. Item 44 addresses the batched-WAL angle for DELETE; this item addresses the scan and index-maintenance overhead that affects both UPDATE and DELETE.

---

## Measured gap (2026-07-15, MM_CRUD_ROWS=20000, matched fsync)

| operation | records | unidb (rec/s) | PG (rec/s) | ratio | WAL B/row | dec/row | cols/row |
|---|---:|---:|---:|---|---:|---:|---:|
| UPDATE bulk (k<N/2) | 10000 | 92 213 | 536 028 | 0.17× | 619 | 1.00 | 8.00 |
| DELETE selected (k>=N) | 20000 | 286 261 | 1 709 329 | 0.17× | 230 | 1.00 | 6.00 |

`dec/row = 1.00` on UPDATE means every matched row is fully decoded (expected — UPDATE reads the old row to produce the new version). `cols/row = 8.00` on UPDATE = 4 cols deformed per row for predicate evaluation (total rows × 1 pred col) + 4 cols decoded for matched rows, then 4 cols written for the new version = (N×1 + half×4) / half ≈ 8 at 50% selectivity. `WAL B/row = 619` for UPDATE is extremely high and is the primary signal — it means the B-tree index for `k` is being updated even when `k` was not modified.

## Root causes

### 1. Unchanged-column index maintenance on UPDATE (primary driver of WAL B/row = 619)
`UPDATE t SET body = 'updated' WHERE k < N/2` does not change `k`, but the executor calls the B-tree `update()` path for the `k` index anyway — tombstoning the old entry and inserting a new one. Each B-tree update writes a full-page-image WAL record per touched leaf page.

**The correct invariant** for unidb's B-tree (which is the only forward resolver — the B-tree leaf stores the RowId that points into the heap): when an UPDATE inserts a new heap version with a new RowId, ANY index that includes the updated row's key must be updated because the RowId it maps to has changed, even if the key value itself is unchanged. This was confirmed and documented when investigating A1 in a prior session — "skip unchanged-column index maintenance was provably incorrect."

**The real fix is not to skip the index update but to reduce its cost:** instead of a tombstone + insert (two leaf-page writes), implement an **in-place RowId patch** in the leaf node. If the key value is unchanged, the leaf entry can be updated in place (`old_row_id → new_row_id`) with a single WAL record containing only the changed bytes — no tombstone, no structural change, no second leaf write. This is valid because the key ordering is unchanged.

### 2. Full-row scan on the UPDATE/DELETE path even with a selective predicate (scan-side overhead)
`matching_rows` falls back to a full heap scan (per the A3 gate for serial ops) at 50% selectivity. Each row requires `deform_row` for the predicate column, producing `N×pred_cols` deform calls for the full table before the filter is applied. At 40k rows, 50% selectivity, 1 pred col: 40k extra column decodes that Postgres avoids via its index-only predicate check.

The A3 gate (item 43) correctly routes serial UPDATE/DELETE to the scan path at 50% selectivity — the gate is right, the cost is real. Reducing it requires either: (a) a vectorised predicate evaluation pass over raw page data (batch-deform the predicate column only, no per-row Literal allocation), or (b) extending parallelism to the UPDATE/DELETE scan path.

### 3. INSERT-new-version MVCC cost on UPDATE
unidb UPDATE = xmax-stamp old version + heap insert new version (two heap operations per updated row). Postgres UPDATE = in-place HOT update on the same page when possible (one heap operation, no index update needed for HOT-eligible columns). HOT requires the old and new versions to be on the same heap page — unlikely after heavy INSERT/DELETE churn, but common in a fresh table.

This is a significant architectural gap. A HOT-equivalent path for unidb would require: ensuring new versions are placed on the same page as old versions when page has free space, and marking the chain so index scans can follow it without a separate index entry update. This interacts with the B-tree's forward-resolver contract (see root cause 1) and is milestone-sized work.

## Phased approach (ROI order)

1. **Phase A — In-place RowId patch for unchanged-key UPDATE.** Eliminates the WAL B/row = 619 spike for body-only updates. Expected WAL B/row to drop to ~100 (new heap version only, no B-tree structural change). Correctness invariant: the patch is only valid when old_key == new_key (no ordering change); any key change still takes the tombstone+insert path.

2. **Phase B — Vectorised predicate scan (batch deform_row for pred column only).** Reduces the `N × pred_cols` deform overhead on the write path for selective ops. Extends B2 decode-pushdown into `matching_rows`.

3. **Phase C — HOT-equivalent UPDATE chain.** Milestone-sized; gated on confirming the B-tree forward-resolver contract can accommodate same-page chains without breaking MVCC visibility. Not scope for this item — file separately if Phase A+B don't close the gap.

## Acceptance criteria

- After Phase A: `WAL B/row` for `UPDATE SET body=... WHERE k<N/2` drops below 150 (from 619). Measured with `Engine::wal_bytes_written_total()` before/after.
- After Phase A: UPDATE bulk reaches ≥ 150k rec/s on 20k rows (from 92k).
- After Phase B: `cols/row` for UPDATE drops from 8.00 toward 4.00+1 (matched rows' full decode + 1 pred col per scanned row / matched rows = still > 4 for selective ops, but total absolute cols decoded should halve).
- All existing UPDATE/DELETE correctness tests + crash harness green (WAL-before-page invariant D5 must hold for the in-place patch path).
- `PROGRESS.md` records before/after with absolute numbers per phase.

## Depends on / builds on

- `src/btree_index.rs` — `DiskBTree::update()` → add `update_rowid_inplace()` for unchanged-key case.
- `src/heap.rs` — `Heap::update()` → thread old/new key comparison signal down to B-tree call site.
- `src/sql/executor.rs` — `exec_update`, `matching_rows` — vectorised predicate deform.
- Item 44 (`44_bulk_delete_batched_wal.md`) — batched WAL mini-txn for DELETE; addresses the mini-txn overhead independent of this item's index-maintenance focus.
- Item 43 (`43_a3_gate_size_aware_selectivity.md`) — SHIPPED; A3 gate correctly routes serial ops to scan, so this item works within the correct gate.
