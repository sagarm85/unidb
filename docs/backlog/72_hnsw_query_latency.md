**Type:** Performance
**Status:** ✅ SHIPPED 2026-07-19 — `HnswL0Cache` (L0 neighbour list cache, item 72) committed in cd94d71; warmup to 7ms at 10k (2× gain). Full target (≤5ms) achieved by adding item 73 (vector hot cache) in the same session. See PROGRESS.md.

# Item 72 — HNSW query latency: in-memory hot layer

## The gap

| Metric | unidb (current) | ffsdb (published) | Gap |
|--------|----------------|-------------------|-----|
| HNSW query latency (10k vectors, dim=128) | 25.19 ms | 113 µs | **223× slower** |
| HNSW insert cost (incremental) | ~14.85 ms | 202 µs | **~73× slower** |
| Recall@10 | 0.947 | 0.966 | similar |

This is the single biggest competitive gap vs the closest comparable product. Root cause is
well-understood from item 63/65 investigation.

## Root cause

unidb's HNSW index is fully on-disk (`DiskHnswIndex`), backed by `DiskBTree`. Every beam search
step requires fetching node pages via `find_node_loc` + `load_node_at` — random I/O into the
mmap-backed page file. At 10k nodes, a single NEAR query performs **~200 unique node page
fetches** (ef_search=200, M=16 neighbours per layer). On Linux with warm mmap pages these are
TLB/page-table walks; on cold pages they are OS page faults.

ffsdb's 113 µs implies their L0 graph (the neighbour lists that dominate beam search at the
bottom layer) is in memory, either fully or as a hot cache keyed by node ID.

## Proposed solution: persistent in-memory L0 cache

Maintain a **process-lifetime in-memory cache of L0 neighbour lists** for the HNSW index.
L0 is the only layer that matters for query latency at typical dataset sizes — upper layers
(L1+) have far fewer nodes and are rarely accessed.

### Structure

```rust
/// Per-index in-memory L0 cache. Lives in Arc<RwLock<...>> on Engine.
/// Populated lazily on first NEAR query; invalidated on HNSW insert/vacuum.
struct HnswL0Cache {
    /// node_row_id → neighbour list (Vec<RowId>, max M_max0 entries)
    neighbours: HashMap<i64, SmallVec<[RowId; 16]>>,
    /// Total size estimate (bytes). Gate: evict or disable above threshold.
    size_bytes: usize,
    /// Generation counter — invalidated on any HNSW write to this index.
    generation: u64,
}
```

### Build strategy

**Option A — Eager warm-up on `CREATE INDEX` / `Engine::open`:** scan all L0 nodes once
at startup and populate the cache. Cost: O(n) page reads at open time. Benefit: first query
is fast.

**Option B — Lazy per-query accumulation:** each NEAR query accumulates the nodes it visits
into the cache. Benefit: zero open cost; reaches steady state after a few queries covering
the graph.

**Option C — Background warm-up thread:** after open, a low-priority background thread
scans L0 nodes into the cache while queries proceed on the disk path. Queries switch to
the cached path once warm.

**Recommendation: Option B (lazy) with Option C (background) as follow-up.** Option A adds
open latency proportional to index size (bad for large indexes). Option B is zero-cost and
converges quickly given that beam search covers most of the reachable graph within a few
hundred queries. Option C can be added later.

### Invalidation

On any HNSW insert or vacuum pass touching the index, increment `generation`. Readers
compare their local generation before trusting cached neighbours; a stale cache entry
forces a disk re-fetch and re-populates. For append-only workloads (no deletes, no vacuum
of vector records), the cache is effectively stable after the warm-up period.

### Size gate

Cap total cache size at `HNSW_L0_CACHE_MB` env var (default: 256 MiB). At 16 neighbours
per node × 8 bytes/RowId = 128 bytes/node, 256 MiB holds ~2M nodes — enough for most
real datasets. Above the cap, fall back gracefully to disk fetch for uncached nodes (no
correctness impact, just the pre-existing latency).

### Expected query latency after fix

| Dataset | Current (disk) | After L0 cache (warm) | Target |
|---------|---------------|----------------------|--------|
| 1k vectors | 8.30 ms | ~0.5–2 ms | ≤1 ms |
| 10k vectors | 25.19 ms | ~1–5 ms | ≤5 ms |
| 100k vectors | timed out (>10 min build) | ~5–20 ms | ≤20 ms |

Exact numbers depend on whether L0 fits in the cache (it will for ≤2M nodes at 256 MiB
cap) and the cost of `SmallVec` lookup vs DiskBTree traversal. Approaching ffsdb's 113 µs
requires keeping vectors in memory too (the distance computations) — that is a separate
follow-up (item 73).

### Vector hot cache (follow-up, item 73)

Even with L0 neighbours cached, each distance computation still requires fetching the
neighbour's vector from disk (`fetch_vector_cached` fallback). For dim=128 that is 512
bytes per node × ~200 nodes per query = ~100 KB of random reads per NEAR query. A
**vector hot cache** (node_id → Vec<f32>) similar to the `NodeCache` used during build
(item 65) but process-lifetime would eliminate these reads. Filed separately as item 73
since it has independent value and different memory pressure implications.

## Acceptance criteria

- NEAR query latency at 10k vectors, dim=128, ef_search=200: **≤5 ms** (5× improvement
  from 25.19 ms; closing toward ffsdb's 113 µs)
- No regression in recall@10 (must stay ≥ 0.94 at 10k)
- Cache respects `HNSW_L0_CACHE_MB` gate; graceful fallback when uncached
- Invalidation on insert: a NEAR query run immediately after an HNSW insert returns the
  correct (updated) result — existing test `hnsw_recall` covers this
- No change to `FORMAT_VERSION` (cache is transient, not persisted)
- Docker bench confirms improvement

## Dependencies

- Builds on item 63 (on-disk HNSW) and item 65 (build-time NodeCache)
- Does NOT require item 67 (async HNSW build) — independent
- Follow-up: item 73 (vector hot cache) for further latency reduction
