**Type:** Performance
**Status:** ✅ SHIPPED 2026-07-19 (PR #161) — `row_count` in `TableDef` maintained on INSERT/DELETE/TRUNCATE; O(1) COUNT(*) fast path, 6.93× vs PG. See `backlog_index.md` row 97 / PROGRESS.md "Item 97". _(Header corrected 2026-07-22 — was never flipped at ship time.)_

# Item 97 — Exact COUNT(*) statistics: O(1) row count from catalog

## Problem

`SELECT COUNT(*) FROM t` currently does a full heap scan at O(n). At 1k rows
the scan itself is fast (~0.05 ms engine time), but the scan still executes and
contributes to the REST path latency. At 100k rows the parallel scan wins
(2.81× PG); at 1k rows it loses (3–5× PG) because parallel workers don't fire
and the overhead-per-page ratio is higher than PG's shared_buffers path.

compare.py reports:
- `COUNT customers`   (1,000 rows): unidb 2.0 ms, PG 0.7 ms  → 3.01× slower
- `COUNT orders`      (2,000 rows): unidb 1.4 ms, PG 0.3 ms  → 4.97× slower
- `COUNT order_items` (5,963 rows): unidb 1.7 ms, PG 0.4 ms  → 4.05× slower

After the plan cache (item 96) removes ~0.2 ms re-parse cost, the remaining
engine gap is ~0.3–1.0 ms. At 1k–6k rows this is real heap scan time.

**Postgres does NOT use `pg_class.reltuples` for exact COUNT(*).** It scans the
heap too. Our parallel scan beats PG at 100k rows because workers amortize
overhead; PG wins at small tables because its shared_buffers scan is extremely
tight. The correct fix is making our COUNT(*) path O(1) — no scan at all.

## What to build

### 1. `row_count` field in `TableMeta` (catalog page)

Add an `i64 row_count` field to the catalog's per-table metadata record:

```rust
struct TableMeta {
    name:        String,
    columns:     Vec<ColumnDef>,
    // ... existing fields ...
    row_count:   i64,   // exact committed row count; maintained on every DML commit
}
```

`row_count` starts at 0 at `CREATE TABLE`. It is:
- **Incremented by +N** when a multi-row INSERT of N rows commits.
- **Decremented by -N** when a DELETE of N rows commits.
- **Unchanged by UPDATE** (row count is stable on UPDATE).
- **Reset to 0** on `TRUNCATE TABLE`.

The update happens inside the mini-txn commit path, under the catalog page
latch, atomically with the WAL commit record. No separate WAL record needed —
the catalog page write is WAL-logged as part of the existing catalog-page-write
mechanism.

### 2. Fast path in `exec_select_count_star`

```rust
// In executor, for: SELECT COUNT(*) FROM t  (no WHERE, no JOIN, no DISTINCT)
if plan.is_count_star_no_filter() {
    let n = catalog.table_meta(table_name)?.row_count;
    return Ok(ResultSet::single_int(n));
}
// Fall through to heap scan for WHERE / JOIN / DISTINCT variants
```

The fast path fires **only** when:
- No `WHERE` clause
- No `JOIN`
- No `DISTINCT`
- No `GROUP BY`

All filtered COUNTs (`COUNT(*) WHERE status = 'delivered'`) still do the heap
scan (or the B-tree path if indexed).

### 3. MVCC correctness

`row_count` reflects the count of **committed** rows as seen by any RC/RR
snapshot with `epoch ≥ commit_epoch`. This is correct for the fast path
because:
- The fast path returns the count at the snapshot's committed epoch.
- Rows inserted by in-flight transactions are not counted (not yet committed).
- Rows deleted by in-flight transactions are still counted (not yet committed).

This is **exactly** what a heap scan at RC isolation would return. The fast
path is semantically equivalent.

**Edge case: REPEATABLE READ with a snapshot taken before recent commits.**
`row_count` always returns the latest committed count, not the snapshot's
count. For exact repeatability, the fast path should be gated to auto-commit
/ RC only, falling back to heap scan for RR transactions. This is a safe
conservative gate.

### 4. Recovery

`row_count` is stored in the catalog page, which is WAL-logged. On recovery
(redo), the catalog page is restored to its post-crash committed state.
No additional recovery logic needed.

### 5. Format compatibility

The `TableMeta` struct is serialised on catalog pages. Adding `row_count`
changes the on-disk layout. **Format bump required**: `FORMAT_VERSION` must
be incremented when this item ships to prevent old unidb from misreading
a new-format catalog page. Migration: on open, if `FORMAT_VERSION` is old,
initialise `row_count = 0` for all tables (sub-optimal for pre-existing data
but safe; a one-time background count-fix pass can be added later).

## Targets

- `SELECT COUNT(*) FROM t` (no WHERE): **< 0.5 ms via REST** regardless of
  table size (plan-cache hit + O(1) catalog read).
- compare.py COUNT queries (3/9): unidb ≤ PG (currently 3–5× slower).
- No regression on filtered COUNT (heap scan / B-tree path unchanged).

## Acceptance criteria

- Unit test: `INSERT 1000 rows` → `SELECT COUNT(*) FROM t` = 1000 (not 0, not
  stale); `DELETE 200 rows` → `SELECT COUNT(*) FROM t` = 800.
- Concurrent test: 8 writers (INSERT/DELETE) + 1 reader (COUNT); reader always
  sees a value ≥ 0 and ≤ total committed inserts; no panic.
- Crash test: insert 500 → crash → recover → `COUNT(*)` = 500 (catalog page
  recovery correct).
- TRUNCATE test: insert 1000 → TRUNCATE → `COUNT(*)` = 0.
- compare.py COUNT queries: ≤ 0.8 ms unidb each (down from 1.4–2.0 ms).
- FORMAT_VERSION bumped; old engine refuses to open new-format file with a
  clear error (`FormatVersionMismatch`).

## ROI

- Three of the nine compare.py queries are `COUNT(*)` variants.
  Together they account for 5.1 ms of unidb's 34.5 ms total (15%).
- Flipping all three from "unidb loses 3–5×" to "unidb wins" changes the
  narrative on the demo's most visible metric.
- Extremely low implementation risk: additive field, safe fallback to heap scan
  on any edge case.
