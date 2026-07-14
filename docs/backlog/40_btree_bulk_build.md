# B-tree index: sort-then-bulk-load backfill

**Type:** Performance
**Status:** NOT STARTED

## Problem

`CREATE INDEX ... USING BTREE` on a populated table is prohibitively slow on
large datasets. On a 5M-row seed (`orders.customer_id`, ~540k rows) the build
runs for several minutes, blocking the single write-serial lock the entire time.

Root cause is in `sql/executor.rs::exec_create_index` (P3.a path, line ~1327):

```rust
// Current: one unsorted insert per row
for (row_id, bytes) in heap.scan(&snapshot, ctx.xid, ctx.pool)? {
    let row = decode_row(&bytes, &table_def.columns)?;
    if let Ok(value) = OrderedValue::try_from(&row[col_idx]) {
        tree.insert(value, row_id, ctx.pool, ctx.wal)?;   // ← hot path
    }
}
```

Two compounding inefficiencies:

1. **One WAL write per row.** Each `tree.insert` is a mini-txn that appends to
   the WAL and may dirty several B-tree pages. 540k inserts = 540k WAL append
   calls + all their page-split side-effects.

2. **Unsorted key order.** Heap scan returns rows in physical (insertion) order,
   not key order. Random-key inserts into a B-tree cause frequent page splits;
   pages end up ~50% full. Every split writes two pages to the WAL instead of
   one.

## Fix: sort-then-bulk-load

The standard approach (PostgreSQL, SQLite, LevelDB) for index backfill:

### Phase 1 — collect
Scan the heap once and collect all `(key, row_id)` pairs into a `Vec`.
Memory cost: `N × (sizeof(OrderedValue) + sizeof(RowId))` ≈ `N × ~24 bytes`.
For 540k rows ≈ 13 MB; for 5M rows ≈ 120 MB — acceptable as a build-time
working set (not persisted, freed immediately after Phase 3).

### Phase 2 — sort
`pairs.sort_unstable_by(|(a, _), (b, _)| a.cmp(b))` — O(N log N) in-memory
sort. Rust's `sort_unstable` is fast enough that this is not the bottleneck.

### Phase 3 — bulk insert (sorted)
Insert the sorted pairs. Because keys arrive in order:
- No page splits during leaf-level insertion (each new key goes to the
  rightmost leaf, which only splits when full — once per page, not once per
  insert in the worst case).
- Pages fill to ~95% instead of ~50%.
- Total WAL writes ≈ `num_pages` (one per leaf page flush) instead of N.

Optional further improvement (not required for first cut):
- Build leaf pages directly, then construct internal pages bottom-up ("B-tree
  bottom-up construction"). Eliminates all splits entirely. More complex; save
  for a follow-up once the sort step is confirmed to land the needed perf.

## Expected impact

| Metric | Before | After (sort) |
|--------|--------|--------------|
| WAL appends (540k rows) | ~540k | ~tens-of-thousands (one per page) |
| Page splits | ~270k | ~0 |
| Page fill ratio | ~50% | ~90-95% |
| Wall time (release, 540k rows) | several min | < 30s (estimate) |

Baseline must be measured on a real 5M seed run before the change is committed
(CONVENTIONS.md §performance: measurement-first).

## Scope

- `src/sql/executor.rs::exec_create_index` — BTree and FullText paths only.
  HNSW already collects into a `sample` Vec before building; it is not affected.
- No API or catalog changes.
- FullText backfill can also benefit: collect `(token, row_id)` pairs, sort
  by token, then insert in order (same logic).

## Acceptance criteria

- [ ] Baseline wall-time measured on `orders.customer_id` (540k rows, release
      build, `UNIDB_BUFFER_POOL_PAGES=1000000`) before the change.
- [ ] After the change, same benchmark runs ≥ 5× faster than baseline.
- [ ] `btree_assisted_select_matches_full_scan_equality_and_range` test still
      passes (correctness — same rows returned as a full heap scan).
- [ ] All existing `CREATE INDEX` integration tests pass.
- [ ] Before/after wall-time table in `PROGRESS.md`.
