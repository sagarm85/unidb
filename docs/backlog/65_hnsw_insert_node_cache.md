**Type:** Performance
**Status:** ✅ SHIPPED 2026-07-18 — per-insert `NodeCache` eliminates repeated DiskBTree
lookups during HNSW beam search. See PROGRESS.md "Item 65".

---

# Item 65 — HNSW incremental insert: per-insert node struct cache

## Problem

Item 63 shipped `DiskHnswIndex` with an `insert_with_cache(build_cache)` path for bulk
`CREATE INDEX` (O(1) vector lookup from a pre-scanned `HashMap<i64,Vec<f32>>`), but the
incremental path (`insert` called from `apply_durable_index_writes` on every SQL INSERT) had
no cache. During `search_layer` (beam search, ef_construction=200):

- For each candidate expanded: `get_l0_nbrs` → `find_node_loc` (DiskBTree, O(log n)) +
  `load_node_at` (page fetch).
- For each of up to M=16 neighbours: `fetch_vector_cached` → `find_node_loc` + `load_node_at`
  again if the neighbour hadn't been expanded yet.

Total DiskBTree lookups per insert at 1k rows: ef × M ≈ 200 × 16 = **3,200**. Each lookup
traverses 2-4 B-tree levels = 6-12 page accesses. Even with all pages in the buffer pool, the
traversal CPU + pin/unpin overhead was dominant.

**Measured W2−W1 before fix:** ~16ms at 1k rows on Docker/Linux (from item 63 bench notes).
**W4/W0 before:** 46.86× (Table 4: unidb 0.06× vs replaced stack — 17× slower).

## Fix

Add `NodeCache = HashMap<i64, HnswNode>` (keyed by `encode_rid(rid)`) as a local variable
in `insert_inner`. The cache accumulates full `HnswNode` structs (vector + L0 neighbours)
across ALL phases of one insert:

- `fetch_vector_cached` (new `&mut NodeCache` parameter): on cache miss, loads full node from
  disk AND stores it in cache. Future `get_l0_nbrs` call for the same node hits cache.
- `get_l0_nbrs` (new `Option<&mut NodeCache>` parameter): on cache miss, fetches node and
  stores in cache. Both vector and L0 neighbours are cached from the first load.

Result: each node is fetched **at most once** per `insert_inner` call (first seen as a
neighbour during beam search, cached; later expanded as a candidate, hits cache for free).

Cache is:
- Created fresh at start of `insert_inner`, dropped at end — NEVER shared across inserts.
- Only used when `build_cache.is_none()` (incremental path). The bulk-build path already has
  the full vector HashMap; adding node struct caching on top of that would use memory for
  minimal additional gain since vectors dominate the lookup cost there.

## Key constraint

The cache must be scoped to ONE insert call. Between separate transactions the graph may have
changed (other writers, committed reciprocal updates) — stale cached L0 neighbours from a
previous insert would cause correctness violations. Rust's ownership model enforces this:
the cache is a local `NodeCache` on the stack of `insert_inner`, not a field of `DiskHnswIndex`.

## Files changed

- `src/hnsw_index.rs`: `NodeCache` type alias + `encode_rid` helper; updated signatures for
  `fetch_vector_cached`, `get_l0_nbrs`, `search_layer`, `apply_reciprocal_l0_to_buf`; new
  `node_cache` local in `insert_inner`; filter_map→sequential-for-loop rewrites in shrink paths
  (needed to allow `&mut NodeCache` borrow across iterations).

## Measurements (native macOS M5 Pro, F_FULLFSYNC)

| rows | W0 (ms) | W1 (ms) | W2 (ms) | W2−W1 (ms) | W4/W0 |
|---:|---:|---:|---:|---:|---:|
| 1k | 3.10 | 3.16 | 37.56 | **34.40** | 16.77× |
| 10k | not measured (see note) | — | — | — | — |

Before fix (native macOS, partial cache): W2=70ms, W2−W1=64ms at 1k rows; W4/W0=17.13×.
After fix: W2 64ms → 34ms at 1k (~47% reduction); W4/W0 17.13× → 16.77×.

**10k note (honest finding):** The W2 pre-grow at 10k rows (10k incremental HNSW inserts via
SQL INSERT on macOS with F_FULLFSYNC) ran for over 22 minutes without completing.
NodeCache eliminates the DiskBTree lookup overhead (~3200→~200 unique fetches) but
the remaining bottleneck is the I/O cost of fetching ~200 unique node pages per insert
as the graph grows: at 10k inserts, the cumulative working set exceeds the hot buffer
pool, driving F_FULLFSYNC-amplified page I/O. Targets (W2−W1 < 2ms, W4/W0 < 5×) are
not met; the remaining gap is beam-search I/O, not DiskBTree CPU overhead.
Docker bench (Linux fdatasync, where absolute numbers are ~3–4× lower) is the
recommended validation for 10k numbers.

Note: macOS F_FULLFSYNC makes absolute numbers ~3-4× higher than Docker/Linux. The W2−W1
delta (pure HNSW CPU, no fsync) should be similar on both platforms.
