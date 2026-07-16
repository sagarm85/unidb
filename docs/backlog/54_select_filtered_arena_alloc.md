# SELECT filtered: arena allocation for row data on parallel scan path (item 45 Lever 3)

**Type:** Performance
**Status:** NOT STARTED

_This is the official tracking item for item 45 Lever 3, deferred when Levers 1 (B-tree range partition, PR #125) and 2 (pre-spawned worker pool, PR #123) shipped._

## Measured gap (`030325`, Docker Linux fsync, 2026-07-16)

| operation | records | unidb (rec/s) | PG (rec/s) | ratio | cols/row |
|-----------|--------:|-------------:|----------:|:-----:|--------:|
| SELECT filtered (k<N) | 10000 | 3,049,245 | 7,265,684 | **0.42×** | 4.00 |

Platform: aarch64 · 18 cores · Linux 6.12.76-linuxkit. Both engines `fsync` (matched durability).

Context: `cols/row=4.00` confirms B2 decode-pushdown is already applied on this path (SELECT already calls `deform_row(pred_upto, pred_needed)`). Items 45 Levers 1+2 gave the parallel scan path a 3–4× throughput improvement. The remaining gap is split between:

1. **Per-row allocator pressure** (this item — addressable in code).
2. **Postgres parallel workers at scale** (architectural — PG fires more workers at 18 cores than our worker pool).

Estimated after Lever 3: **~0.50–0.58× PG** (+20–38%). The residual gap beyond ~0.55× reflects Postgres's parallel query planner using more workers than our capped pool; it cannot be closed by allocation tuning alone.

## Root cause

Every matched result row is currently represented as a `Vec<Literal>` allocated per-row on the Rust global allocator. For a filtered SELECT returning N matched rows:

- N allocations of `Vec<Literal>` (heap metadata, 24 bytes each).
- For each TEXT/BLOB column value: an additional `String` allocation (heap metadata + data copy).
- Total: O(N × cols) allocator calls on the hot parallel scan path, each entering and exiting the global allocator's lock or thread-cache.

At 3M rec/s (current), with 4 columns/row, this is ~12M allocator round-trips per second from a single query. Each call is cheap in isolation but aggregates to a significant fraction of total CPU time under the parallel worker pool.

The fix is **per-query arena allocation**: allocate a single slab at query start, bump-allocate all row data from it, and free the whole slab at query end. No per-row free, no per-row allocator overhead.

## Plan

### Phase A — Per-query bump allocator for row data
Introduce a `RowArena` struct (a simple bump allocator over a `Vec<u8>` slab, not exposed outside the query layer):

- Allocated once at `exec_select` entry with a capacity estimated from `expected_rows × avg_row_bytes`.
- `Literal::Text` values are stored as `&'arena str` slices into the arena buffer (zero-copy after the initial decode).
- `Vec<Literal>` per row becomes a fixed-size array or a slice into a pre-allocated `Vec<Vec<Literal>>` for the result set.
- The arena is dropped at query end, freeing all row memory in one `dealloc`.

### Phase B — Worker-local arenas
Each parallel scan worker gets its own `RowArena`. Workers produce slices; the coordinator merges slices without re-allocation. Eliminates cross-worker arena contention for large result sets.

## Acceptance criteria

- `SELECT filtered` reaches ≥ 0.48× PG at 10k rows (from 0.42×).
- `cols/row` remains ≤ 4.00 (B2 decode-pushdown must not regress).
- No change to query result correctness (existing SELECT tests pass).
- Peak RSS measured before/after: should stay flat or decrease (arena re-use reduces fragmentation vs per-row allocation).
- `PROGRESS.md` records before/after with absolute rec/s numbers and RSS.

## Depends on / builds on

- `src/sql/executor.rs` — `try_exec_select_btree`, parallel worker dispatch.
- Item 45 Levers 1+2 (`45_select_filtered_parallel_btree_scan.md`) — SHIPPED (PRs #123, #125). This item continues the same scan path.
- `src/sql/parallel_scan.rs` — parallel worker pool (Lever 2); Phase B of this item touches the same module.

## Parallel note

This item is independent of items 51 (hash join), 52 (decode pushdown on write path), and 53 (FK re-check). It can be developed in a separate worktree without conflicts. The only dependency is that Levers 1+2 are already merged (they are).

## What this does NOT fix

- The architectural gap vs PG at very large row counts where PG's parallel degree dominates. That requires increasing our worker pool ceiling or adding dynamic degree scaling, which is a separate governance item.
- UPDATE/DELETE scan allocation — items 52 addresses those paths.
