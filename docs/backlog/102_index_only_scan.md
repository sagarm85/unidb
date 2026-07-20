**Type:** Performance
**Status:** ⏳ NOT STARTED (filed 2026-07-20)

# Item 102 — Index-only scan / covering index

## Problem

Every B-tree index scan today does two fetches per matching row:
1. B-tree leaf scan → yields `(key_bytes, RowId)`
2. Heap fetch → `pool.fetch_page(row_id.page_id)` → decode full row

The heap fetch is the bottleneck for SELECT filtered at ~0.55× vs Postgres.
Even when all projected columns are available in the B-tree leaf (the key
columns themselves), unidb goes to the heap anyway.

Postgres avoids this with *index-only scans*: when the query's projection is a
subset of the index columns, rows are returned from the index leaf without ever
reading the heap page. For tight `SELECT col FROM t WHERE col = val` queries
this eliminates all heap I/O — turning a two-page-fetch into a one-page-fetch.

## Two-phase plan

### Phase A — Key-column projection (zero format change) ← start here

When the SELECT projects **only the indexed key column(s)**, return the key
value directly from the B-tree leaf without fetching the heap page.

**Example:**
```sql
CREATE INDEX ON events (user_id);
SELECT user_id FROM events WHERE user_id = 'alice';
-- B-tree leaf already has 'alice'; heap fetch unnecessary.
```

**Scope:** Optimizer detects: all SELECT columns ∈ index key columns AND no
non-indexed columns referenced. The B-tree scanner returns the key bytes in
place of the RowId; the executor decodes them as a row without hitting the heap.

**Format impact:** None — the B-tree leaf already stores key bytes.
No `FORMAT_VERSION` bump. No new syntax.

**Acceptance target (corrected after implementation):** `SELECT <indexed_col> FROM t WHERE
<indexed_col> = val` skips `deform_row` (column decode) and returns the key value
directly from the B-tree leaf. A lightweight `heap.get()` is still performed for
MVCC visibility — B-tree leaves retain stale entries for dead tuples until vacuum
runs, so the heap page must be touched to confirm row liveness. **Phase A savings
are CPU (deform_row eliminated), not I/O (heap page fetch remains).** The
`IDX_ONLY_ROWS` counter increments for every row returned via the fast path.
True zero-heap-fetch requires a visibility map (Phase B work item).

### Phase B — Covering index with INCLUDE columns (format change)

Support `CREATE INDEX ON t (user_id) INCLUDE (id, body)` — stores non-key
columns in the B-tree leaf, enabling index-only scan for any query whose
projected columns are a subset of `(key_cols ∪ include_cols)`.

**Example:**
```sql
CREATE INDEX idx ON events (user_id) INCLUDE (ts, payload);
SELECT ts, payload FROM events WHERE user_id = 'alice';
-- No heap fetch needed: ts and payload are in the leaf.
```

**Format impact:** B-tree leaf format changes to `(key_bytes | include_bytes |
RowId)`. Requires `FORMAT_VERSION` bump. Existing indexes (without INCLUDE)
read normally (include section is zero-length).

**Acceptance target:** Covering-index SELECT shows 0 heap fetches; throughput
≥ 2× non-covering for the same query. Docker bench SELECT filtered improves by
≥ 15% when the bench is run with a covering index on the filter column.

## Why Phase A first

Phase A is zero-format-risk and can ship independently. It already covers a
common analytics pattern (`SELECT DISTINCT col`, `SELECT COUNT(col)`,
`SELECT col FROM … WHERE col = val`). Phase B is the bigger win but requires
careful format migration and a FORMAT_VERSION bump.

## Bench impact (honest)

The current Docker bench "SELECT filtered" workload projects **all columns**
(measures full-row throughput). Phase A will **not move that bench number** —
it only helps queries projecting only indexed columns.

Phase B (covering index) CAN move the bench number IF the bench query is
changed to use a covering index. If the bench is not modified, Phase B also
does not appear in the headline Table 3 numbers.

The real value of this item is **query latency for common access patterns**,
not the current micro-benchmark. Specifically:
- Multi-tenant `SELECT user_id FROM sessions WHERE user_id = $1` — auth lookup
- `SELECT COUNT(*) FROM events WHERE category = 'error'` — already O(1) via
  item 97; covering index would eliminate the index scan entirely for filtered counts
- Analytics `SELECT DISTINCT tag FROM logs WHERE tag != ''`

## Implementation sketch (Phase A)

In `src/sql/optimizer.rs`:
- In `try_exec_select_btree` detection: add check `all_projected_cols ⊆ index_key_cols`.
- Set a new plan flag `index_only: bool` on the B-tree scan node.

In `src/sql/executor.rs` (or `query_exec.rs`) B-tree scan loop:
- When `index_only`: emit a row built from the key bytes directly (using the
  B-tree column schema), skip `pool.fetch_page(row_id.page_id)`.
- When `!index_only`: existing path unchanged.

New internal counter: `IDX_ONLY_ROWS` (alongside `HEAP_FETCHES`) to verify
in tests that heap fetches are truly zero.

## Dependencies

**Phase A: can start immediately, parallel to items 67-92.**
- `src/btree.rs` and the B-tree scan loop are NOT in the 67-92 diff.
- `src/sql/optimizer.rs` IS touched by item 51 (hash join detection) in 67-92,
  but at a different location (`try_build_hash_table` planner vs index-only
  detection). Merge conflict is resolvable (likely a 5-line context diff).
- `src/sql/executor.rs` IS touched by item 67 (ExecCtx.hnsw_tx) and item 68
  (hint bits in visibility check). Those touch `exec_insert` and `get_visible`;
  index-only scan touches the B-tree scan path (`exec_select_btree`).
  Different functions — manageable merge conflict.

**Phase B: wait for 67-92 to merge first** (cleaner FORMAT_VERSION baseline).
- Phase B requires a FORMAT_VERSION bump; the bump is simpler after 67-92 lands
  to avoid a double-bump in flight.
- Phase A is the higher-ROI step and has zero dependency.
