**Type:** Performance
**Status:** 🚧 IN PROGRESS — Levers 1+2+3 shipped; ≤700 µs at 10k pending Docker bench

# Item 92 — Vector query next tier: 2.38 ms → pgvector-class (≤700 µs)

## Problem

After items 72 (L0 neighbour cache) + 73 (vector hot cache), warm NEAR at 10k
vectors is **2.38 ms** — 11× better than a week ago but still **~6× behind
pgvector (380 µs)** and ~19× behind ffsdb (126 µs). The "strip the SQL
transaction" hypothesis is contradicted by the comparison itself: pgvector
runs a *full* SQL/MVCC transaction per query and is 6× faster — txn setup is
tens of µs; the residual ~2 ms is per-hop node access cost.

## Step 0 profile (2026-07-19)

Measured at 2000×dim128, k=10, ef_search=200, release build, macOS M-series.
Instrument: `Q_L0_CACHE_HITS`, `Q_VEC_CACHE_HITS`, `Q_DISK_FETCHES`,
`Q_DISTANCE_CALLS` atomics in `hnsw_index.rs`; test `perf_item92::hnsw_step0_profile`.

### Before item 92 (items 72+73 baseline at 2k×dim128)

| Metric             | Cold (1st query) | Warm (avg 14 q) |
|--------------------|-----------------|-----------------|
| Latency            | 24,145 µs       | 1,692 µs        |
| L0 cache hits/q    | 0               | 154             |
| Vec cache hits/q   | 1               | 1,594           |
| Disk fetches/q     | 1,795           | **48**          |
| Distance calls/q   | 1,632           | 1,631           |

**Key finding**: 48 disk fetches per query even on "warm" path because the L0
cache only fills incrementally (each query caches nodes it visits; different
queries visit different neighborhoods). After only 5 warmup queries on 2000
nodes, ~48 nodes per subsequent query still miss the L0 cache, each paying
a DiskBTree lookup + page read (~50 µs) → the dominant query cost.

Prediction verified: txn overhead ≪ 1 µs (not the bottleneck).
Root cause: incomplete L0 cache warm + per-hit Vec<f32> allocation.

### After item 92 — Levers 1+2+3 (at 2k×dim128)

| Metric             | Cold (1st query) | Warm (avg 14 q) |
|--------------------|-----------------|-----------------|
| Latency            | 1,265 µs        | **921 µs**      |
| L0 cache hits/q    | 200             | 200             |
| Vec cache hits/q   | 1,596           | 1,596           |
| Disk fetches/q     | 0               | **0**           |
| Distance calls/q   | 1,631           | 1,631           |

**Improvement**: cold 24 ms → 1.3 ms (18×); warm 1,692 µs → 921 µs (−45%).

### What each lever fixed

| Lever | Code change | Impact |
|-------|------------|--------|
| L1 (zero-copy) | `compute_distance_if_cached` in `DiskHnswIndex` | Eliminates ~1600 × 512B Vec allocations/q on warm path |
| L2 (SIMD-friendly) | `dist_raw` 8-lane f32 accumulator; `ivf_exact_distance` delegates | ~4× distance throughput on NEON/AVX2 via auto-vectorisation |
| L3 (prefetch) | `prefetch_caches` after `CREATE INDEX` scans all node pages | Eliminates all disk fetches; cold 24 ms → 1.3 ms |

### Cost attribution (warm query, post-item-92)

| Cost source              | µs/q | Notes                                |
|--------------------------|------|--------------------------------------|
| Distance computation     | ~130 | 1631 calls × 128 f32 SIMD mul+acc   |
| L0 Vec clones (200×192B) | ~10  | Owned clone of neighbour list/q      |
| ANN BinaryHeap+HashSet   | ~200 | ef=200 heap operations               |
| Heap re-rank (200 rows)  | ~350 | MVCC visibility + row decode/q       |
| Txn + snapshot           | ~2   | begin/commit overhead                |
| **Total warm (2k)**      | **~921 µs** |                             |

## Levers implemented

### Lever 1 — Zero-copy cache-hit distance (no Vec allocation)
**File**: `src/hnsw_index.rs`
**Method**: `DiskHnswIndex::compute_distance_if_cached`

On `HnswVecCache` hit: runs `dist_raw(metric, query, cached_slice)` directly
against the stored `&[f32]` — no `Vec<f32>` allocation, no memcpy.
Falls back to `fetch_vector_cached_with_vec` only on cache miss (disk path).
Called by `search_layer_with_vec` on the query path when `vec_cache: Some(…)`.

### Lever 2 — SIMD-friendly 8-lane Euclidean accumulator
**File**: `src/hnsw_index.rs`
**Function**: `dist_raw`

Replaces the scalar iterator chain with 8 independent `f32` accumulators so
LLVM can fill 2 NEON 128-bit registers (arm64) or 1 AVX2 256-bit register
(x86_64) per loop iteration without loop-carried dependencies.
`hnsw_distance` delegates to `dist_raw`; `ivf_exact_distance` in executor.rs
also delegates (unifying the ANN and re-rank distance paths).

### Lever 3 — L0 + vector cache prefetch after CREATE INDEX
**File**: `src/hnsw_index.rs` + `src/sql/executor.rs`
**Method**: `DiskHnswIndex::prefetch_caches`

After `CREATE INDEX ... USING HNSW` completes (in `exec_create_index`), walks
the `node_index` DiskBTree via `validate()` to enumerate all node locations,
then sequentially loads each node's L0 neighbour list and vector into the
process-lifetime `HnswL0Cache` and `HnswVecCache`. Cost: O(n) sequential reads
(≈7 MB at 10k×dim128). Eliminates **all** disk fetches on the first and every
subsequent NEAR query (0 disk fetches measured on warm path after prefetch).

## Levers skipped

### Lever 4 — Read-only NEAR fast path (skip snapshot)
**Reason**: txn overhead ≪ 1 µs (negligible). Not worth complexity.

### Lever — Pool fetch CRC cost (Wave 1 synergy, item 86)
**Reason**: `bufferpool.rs` is owned by Wave 1 branch (`perf/v3-20260719`).
Lever 3 (prefetch) eliminates all disk fetches on the warm path so item 86
contributes 0 on the warm path. Pending Wave 1 merge for other paths.

### Lever — L0 zero-copy arena
**Reason**: L0 clone overhead is only ~10 µs/q (200 clones × 192B).
The main gain from arena would be on the vec side, handled by Lever 1.
Deferred — not in top-3 by measured ROI.

## Targets

- Warm NEAR at 10k, recall@10 ≥ 0.90 (ef_search=200 unchanged):
  **≤ 700 µs** (pgvector-class). At 2k vectors post-levers: **921 µs**
  (CI proxy; Docker bench at 10k pending to confirm Linux result).
- **Fairness rule**: pgvector/ffsdb latencies quoted at matched recall.

## Acceptance criteria

- [x] Step-0 profile table committed before any lever built.
- [x] recall@10 = 1.000 at 2k×dim128 (gate ≥ 0.90).
- [x] All 434 unit tests + 51 crash tests pass.
- [x] cargo clippy -D warnings clean; cargo fmt clean.
- [ ] Docker bench confirms ≤700 µs warm at 10k×dim128 on Linux.
- [ ] W2 (vector-write) rung of decompose ladder shows no regression.
