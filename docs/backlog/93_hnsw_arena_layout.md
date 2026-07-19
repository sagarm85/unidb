**Type:** Performance
**Status:** ⏳ NOT STARTED — follow-up to items 72 + 73 + 92

# Item 93 — HNSW L0 arena layout: eliminate per-neighbor Vec clone

## Problem

After items 72/73/92, warm NEAR at 2k is **921 µs** (down from 25,190 µs).
The dominant remaining cost is per-neighbor `Vec` clone during beam search:

```
200 hops × M=16 neighbors/hop × ~100 ns per Vec<RowId> clone ≈ 320 µs
```

The current `HnswL0Cache` stores `HashMap<i64, Vec<RowId>>` (node_id → neighbor
list). Every beam-search step calls `.get(node_id)` → `.to_vec()` or iterates
over a cloned reference — heap allocation per node visit, hot in the
allocator. The HashMap itself costs ~50 ns per lookup (pointer indirection +
hash), adding another ~10 µs across 200 hops.

Item 92's "zero-copy node layout" lever (listed but not implemented) is the
fix. This item implements it properly.

## Architecture (what to build)

Replace the HashMap-backed L0 cache with a flat arena:

```rust
struct L0Arena {
    /// Sequential node index, assigned at first HNSW insert/prefetch.
    /// node_id (i64 RowId) → node_idx (u32)
    node_idx_map: HashMap<i64, u32>,
    /// Contiguous neighbor list arena.
    /// Layout: for node_idx=k, neighbors are at
    ///   &arena_data[arena_offsets[k] .. arena_offsets[k+1]]
    arena_data:    Vec<i64>,   // neighbor RowIds, packed
    arena_offsets: Vec<u32>,   // len = num_nodes + 1 (prefix sum)
    num_nodes:     u32,
}
```

- **Lookup**: `node_idx_map.get(node_id)` → `k` → `&arena_data[offsets[k]..offsets[k+1]]`
  — a single pointer dereference, zero copy, zero allocation.
- **Mutation (INSERT)**: append new neighbor list at the end; record offset.
  If HNSW INSERT modifies an existing node's neighbor list (re-wiring), update
  in place or invalidate + append (simpler: mark old entry as tombstone, append
  updated list, update `arena_offsets[k]`).
- **Rebuild vs incremental**: two valid strategies:
  - *Rebuild*: arena is rebuilt entirely by `prefetch_caches` (already called
    after `exec_create_index`); incremental updates via INSERT/DELETE are
    tombstone-and-append; a background compaction runs if fragmentation > 50%.
  - *Incremental-only*: no rebuild; INSERT appends; delete tombstones.
  - **Recommendation: rebuild on `prefetch_caches`, incremental append for INSERT,
    tombstone for delete.** Fragmentation is bounded by the insert rate.
- **Thread safety**: arena behind `RwLock<L0Arena>`; beam search takes a
  `read()` guard, INSERT takes a `write()` guard for the duration of the
  neighbor-list update.

## Interaction with item 92's L3 prefetch

`prefetch_caches` already walks all HNSW nodes to populate `HnswL0Cache`.
During that walk, assign each node a `node_idx` and build the arena. After
prefetch, all beam-search hops pay zero allocation.

## Targets

- Warm NEAR at 10k, recall@10 ≥ 0.90 (ef_search=200 unchanged):
  **≤ 600 µs** (expected: 921 − 320 µs clone − 10 µs HashMap = ~590 µs).
- No regression in W2/W3/W4 (vector-write rungs of the decompose ladder).
- No `unsafe` added (arena uses safe `Vec<>` slicing).

## Acceptance criteria

- Step-0 profile: instrument per-hop allocation cost before building; confirm
  clone elimination accounts for ≥ 250 µs of the warm path.
- ≤ 600 µs warm at 10k with recall gate held; Docker bench confirms on Linux.
- Existing crash tests (all `P6*` + `P_vec_*`) pass unchanged.
- No net RSS increase: arena is flat bytes vs a HashMap of heap-allocated Vecs;
  expected RSS neutral or slight win.

## ROI rationale

- Estimated gain: **−300–400 µs** (dominant remaining cost after item 92).
- Fully independent of CRUD items (87/88/89/90) — only touches `hnsw_index.rs`.
- Synergizes with item 94 (skip-txn): arena eliminates allocation overhead;
  skip-txn eliminates snapshot overhead; together → pgvector-class (~380 µs).
- Lower risk than item 94 (no design-decision change, no new fast path).

**Do item 93 before item 94** — arena removes the biggest latency component;
item 94's ~75 µs gain only matters once 93 lands.
