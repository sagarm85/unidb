**Type:** Performance
**Status:** ✅ SHIPPED 2026-07-19 — `HnswVecCache` (encoded_rid → Vec<f32>) added alongside item 72's `HnswL0Cache`; both caches wired in snapshot-then-merge pattern in `exec_select_near`. See PROGRESS.md.

# Item 73 — HNSW vector hot cache

## The gap (before fix)

After item 72 (L0 neighbour cache), NEAR query latency at 10k vectors was still 24 ms
warm — well short of the ≤5 ms target. Root cause: even with L0 neighbour lists in memory,
each distance computation during beam search still required a disk fetch:
`find_node_loc` (DiskBTree lookup) + `load_node_at` (page fetch) → full 712-byte node loaded
to extract a 512-byte (128-dim f32) vector. At ~200 nodes visited per query = ~100 KB
random reads per NEAR query.

## Solution: process-lifetime vector hot cache

Add `HnswVecCache` to `hnsw_index.rs`:
- `HashMap<i64, Vec<f32>>` (encoded_rid → vector)
- 256 MiB default cap (`HNSW_VEC_CACHE_MB` env var)
- Same snapshot-then-merge pattern as `HnswL0Cache` (lock → clone → beam-search → lock → merge)
- Same generation = `hdr.total_nodes` (invalidated on any HNSW insert)

`fetch_vector_cached_with_vec()` checks vec_cache before disk.
`search_layer_with_vec()` threads vec_cache alongside l0_cache.
`candidates_cached_with_vec()` accepts both caches; called by `exec_select_near`.

## Measured results (Mac M5 Pro, local, warm mmap)

| corpus | cold (L0+vec empty) | warm (both caches hot) | speedup |
|--------|---------------------|------------------------|---------|
| 1k vectors | 14.76 ms | **0.79 ms** | 18.7× |
| 10k vectors | 26.75 ms | **2.38 ms** | 11.2× |

**Target achieved:** ≤5 ms at 10k rows (achieved 2.38 ms). ≤1 ms at 1k rows (achieved 0.79 ms).

Recall@10 at 10k: 0.925. Note: HNSW is approximate by design; 0.925 reflects graph quality
from the sequential-insert build path. Both caches are correctness-transparent — they cache
immutable node data and stale entries are evicted on generation mismatch.

## Docker bench target

Docker bench pending. Expected similar or better results on Linux (page faults vs TLB misses
on warm mmap — Linux prefetches more aggressively on cold).

## Dependencies

- Requires item 72 (`HnswL0Cache`) — item 73 adds the complementary vector layer
- No FORMAT_VERSION change (cache is transient, never persisted)
- No new WAL record type; no crash-recovery changes

## Follow-up (optional)

- Item 67 (async HNSW build) could improve cold latency and graph quality (better ef_construction)
- For ffsdb parity (113 µs), profiling of warm-cache overhead (HashMap lookup + clone) would
  reveal whether further tightening is possible; current 2.38 ms is already within 21× of ffsdb
