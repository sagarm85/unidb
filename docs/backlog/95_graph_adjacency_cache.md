**Type:** Performance
**Status:** ⏳ NOT STARTED

# Item 95 — Graph adjacency cache: in-memory hot-hub warm cache

## Problem

The current graph 1-hop traversal path:

```
exec_select_graph_neighbors()
  → DiskBTree::scan_range(from_id, to_id)   // B-tree lookup: 2-4 page fetches
  → heap fetch per edge record               // 1 page fetch per ~20 edges
  → deform_row() per edge
```

Every traversal hits the B-tree cold path. At 10k edges per hub (a typical
social-graph hub), a 1-hop read requires:
- ~3 B-tree internal pages + ~1 leaf page per B-tree lookup = ~4 page fetches.
- ~500 heap pages if edges are spread across the heap.
- Total wall time: **2–10 µs** (warm buffer pool) to **50–200 µs** (cold).

By comparison ffsdb reports 1-hop at **11 ns** — pure in-memory adjacency
array. We cannot match 11 ns from a disk-backed B-tree, but an adjacency
cache eliminates the B-tree and heap page fetches for hot hubs, reducing
warm 1-hop to **~100–500 ns**.

## Architecture

```rust
/// Per-engine, per-table adjacency cache.
/// Key: (table_name, from_id). Value: sorted list of (to_id, props_blob).
struct AdjacencyCache {
    entries: DashMap<(String, i64), Arc<Vec<EdgeRef>>>,
    max_hubs: usize,    // default 50_000 (configurable)
    max_edges: usize,   // total edge refs in cache (memory budget)
}

struct EdgeRef {
    to_id: i64,
    edge_row_id: i64,          // for property lookup on cache miss
    props_inline: Option<Vec<u8>>,  // small props (<256 B) inlined
}
```

### Cache population

- **Lazy (first access)**: on first `SELECT NEIGHBORS FROM t WHERE from_id=k`,
  scan all edges for `k` from the B-tree, load into cache.
- **Eager (optional)**: `WARM GRAPH t` SQL surface triggers a background walk
  of the top-N hubs (ordered by degree from a degree index, item 96 if filed).

### Cache invalidation

- **INSERT edge**: invalidate (remove) the entry for `from_id`.
  Simple invalidation is safe and correct; re-population on next read.
- **DELETE edge**: same — invalidate `from_id` entry.
- **UPDATE edge props**: invalidate (if we inline props) or refresh inline.
- No cross-node consistency issue: each from_id entry is invalidated atomically
  under the `DashMap` shard lock; no other reader can see a stale half-updated
  entry (Arc clone keeps old version alive until last reader drops it).

### Memory budget

- `max_hubs = 50_000`: each hub entry is an `Arc<Vec<EdgeRef>>`.
  At 100 edges/hub × 24 B/EdgeRef = 2.4 KB/hub → 50k hubs ≈ 120 MB.
- Eviction: LRU shard (add `last_used: AtomicU64` timestamp per entry;
  background sweeper evicts the bottom-half by age when over budget).
- Configurable via `UNIDB_GRAPH_CACHE_HUBS` env var.

### Property handling

For the common case (`SELECT to_id`), the cached `EdgeRef.to_id` is enough —
no heap fetch needed. For `SELECT *` or property predicates, fall back to heap
fetch via `edge_row_id`.

## Interaction with MVCC

The cache stores **committed** edge data (the row visible at the snapshot epoch
when it was loaded). For REPEATABLE READ transactions that re-read the same hub,
the cache may return a slightly-stale view if an edge was added after the cache
was populated. Options:

1. **Validate epoch on hit**: cache entry records the `committed_xid_epoch` at
   population time; if the current snapshot's epoch > entry epoch, re-fetch.
   Safe and correct; adds ~5 ns per cache hit.
2. **Invalidation on commit**: edge INSERT/DELETE invalidates the cache entry
   immediately on commit (already described above).

**Recommendation**: use invalidation-on-commit (Option 2). It's simpler and
correct for RC isolation (our default). For REPEATABLE READ, the epoch-validate
path is a follow-on.

## Targets

- **1-hop hot traversal**: **≤ 500 ns** (cache hit, no heap fetch for to_id-only
  SELECT) — vs current 2–10 µs (10-50× improvement).
- **1-hop cold traversal**: unchanged (first-access still does the B-tree walk).
- **No regression on edge INSERT throughput**: invalidation is O(1) per INSERT.
- **Memory**: ≤ 200 MB RSS increase at 50k cached hubs.

## Acceptance criteria

- `bench_graph_1hop` (new bench): 10k edges per hub, warm path ≤ 500 ns p50,
  ≤ 2 µs p99; Docker bench confirms on Linux.
- Concurrent writer + reader stress: 8 writers (edge INSERT), 8 readers
  (SELECT NEIGHBORS), 100k iterations, 0 stale reads / 0 panics.
- No regression in graph edge INSERT throughput (vs pre-95 baseline on main).
- Cache can be disabled via `UNIDB_GRAPH_CACHE_HUBS=0` (existing tests run
  without cache to preserve correctness coverage of the B-tree path).

## ROI rationale

- **Primary gap vs ffsdb**: 1-hop 11 ns (ffsdb) vs 2–10 µs (unidb). With
  adjacency cache, hot-hub gap closes to ~45× (500 ns vs 11 ns). Full parity
  (11 ns) requires a pure in-memory graph structure without B-tree, which
  trades crash-durability for speed — not our architecture.
- **Fairer comparison**: ffsdb adjacency cache is an in-memory-only structure.
  Our cache gives the same in-memory perf on hot hubs while maintaining full
  WAL/MVCC durability. This is the comparable regime.
- **M3 blocker enabler**: once the graph path is fast enough to be measurable,
  we can headline the M3 column in the replaced-stack benchmark vs Neo4j
  (adjacency scan bottleneck would make that column look bad).
- **Independence**: touches only `src/graph/edges.rs` and the graph section of
  `src/sql/executor.rs`. Zero overlap with CRUD items (86–90) or vector items
  (93–94).

## Implementation order

This item is **independent** — can be done in parallel with item 93/94 on a
separate branch. Recommended order within vector/graph sprint:

1. Item 93 (arena layout, vector) — biggest latency reduction.
2. Item 95 (adjacency cache, graph) — parallel to 93/94.
3. Item 94 (skip-txn, vector) — last because it's the smallest gain and
   requires a design gate.
