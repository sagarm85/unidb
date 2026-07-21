**Type:** Performance
**Status:** ✅ SHIPPED 2026-07-21 — Levers 1+2+3+5+7; native warm 10k = ~900 µs
(from 2,091 µs at session start; 2.38 ms pre-item-72). **Acceptance revised
≤700 µs → ≤1 ms with explicit user sign-off 2026-07-21** (recorded in
PROGRESS.md "Item 92 — Vector query Levers 5+7"): remaining micro-levers
floor at ~700–750 µs, and the pgvector-class 380 µs tier requires
quantization/graph-quality work — filed as **item 106**. Lever 6 (fast
hasher) rejected on A/B evidence; see "2026-07-21 session" below.

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

- ~~Warm NEAR at 10k, recall@10 ≥ 0.90 (ef_search=200 unchanged):
  **≤ 700 µs** (pgvector-class).~~
  **Revision (2026-07-21, user sign-off):** target is **≤ 1 ms warm at
  10k×dim128, recall@10 ≥ 0.90** — achieved at ~900 µs (±2 µs across runs).
  The original ≤700 µs was set before the 10k phase split existed; measured
  floors show the remaining micro-levers land ~700–750 µs at best, and the
  pgvector-class 380 µs tier needs quantization/graph-quality work →
  **item 106**. Not a silent rewrite: evidence in the 2026-07-21 session
  section below, sign-off recorded in PROGRESS.md.
- **Fairness rule**: pgvector/ffsdb latencies quoted at matched recall.

## Acceptance criteria

- [x] Step-0 profile table committed before any lever built.
- [x] recall@10 = 1.000 at 2k×dim128 (gate ≥ 0.90); 0.900 at 10k (gate ≥ 0.90).
- [x] All unit tests + crash tests pass (2026-07-21: 30 binaries green, 54/54 crash).
- [x] cargo clippy -D warnings clean; cargo fmt clean.
- [x] Warm NEAR ≤ 1 ms at 10k×dim128 native (revised gate — ~900 µs measured).
- [ ] Docker/Linux NEAR spot-check + W2 (vector-write) rung no-regression —
      folded into the consolidated bench run (2026-07-21, in flight).

## 2026-07-21 session — Levers 5–7 (10k-scale round)

### Step-0 re-profile at 10k (levers 1–3 did not scale)

Native 10k×dim128 warm NEAR on main (post levers 1+2+3, items 93/67 in):
**2,091 µs** — the 2k gains did not carry (distance calls 1,631 → 3,748/q,
plus 1,257 µs *unattributed*). Recall@10 = 0.900, exactly at the gate → the
`ef_search` knob is pinned; no headroom to trade recall for speed.

### Lever 5 — O(1) cache snapshots (Arc copy-on-write) ✅ SHIPPED

Root cause of the unattributed block: `exec_select_near` **deep-cloned the
entire per-index cache every query** — full L0 arena + a 10k-entry
`HashMap<i64, Vec<f32>>` (~7 MiB + 10k allocations), then walked all 10k
entries again in `merge_from`. O(corpus) per query: ~300 µs at 2k, ~1.3 ms at
10k, ~15 ms at 100k (worse than no cache). The lock-free-during-I/O rationale
predated Lever 3; after prefetch the warm path does zero I/O, so the clone
bought nothing.

Fix: `HnswVecCache.vectors` and `HnswL0Cache.arena` behind `Arc`; mutations
via `Arc::make_mut` (copy-on-write, only on an actual cache miss); executor
compares `storage_ptr()` before/after and **skips merge-back entirely** when
the search inserted nothing; `merge_from` gains ptr-equal → no-op and
empty-self → O(1) Arc-adopt fast paths.

**Measured: warm 10k 2,091 → 895.5 µs (−57%); cold 2,331 → 1,499 µs;
counters and recall identical (pure overhead removal).**

### Lever 6 — fast hasher (FxHash-style) ❌ REJECTED (measured wash)

Hypothesis: ~7k SipHash ops/q (vec-cache get + visited set) cost 150–300 µs.
A/B with 3 runs each: FastHash ~996 µs vs SipHash ~992 µs — statistically
indistinguishable. Hashing is not the bottleneck (the cost is the memory
pointer-chase, not the hash function). Reverted entirely; do not re-attempt
without new evidence.

### Phase attribution (new, permanent): ANN vs re-rank timers

`Q_ANN_NANOS` / `Q_RERANK_NANOS` atomics in `exec_select_near`, printed by
`tests/perf_item92.rs`. Warm 10k split: **ANN 586–610 µs (66%) · re-rank +
project 222 µs (25%) · parse/plan/snapshot ~74 µs**. Within ANN, distance
math ≈150 µs (3,748 calls × ~40 ns SIMD); the rest is graph-traversal
bookkeeping over a 5 MiB working set.

### Lever 7 — contiguous vector slab (`VecArena`) ✅ SHIPPED

Same pattern as item 93's `L0Arena`, applied to vectors: one flat `Vec<f32>`
slab + key→slot map, replacing 10k scattered 512 B `Vec<f32>` allocations.
Drop-in inside `HnswVecCache` thanks to Lever 5's encapsulation.

**Measured: warm 10k = 897.9 / 899.7 / 902.1 µs — mean ~900 µs (~9% below
Lever-5-alone mean ~990 µs) and variance collapsed from ±120 µs to ±2 µs.**
The locality hypothesis mostly did not pay (ANN still ~605 µs — random access
over 5 MiB is TLB/cache-miss-bound regardless of layout); the honest wins are
determinism, allocator pressure (10k fewer live allocations per index), and
single-memcpy COW clones.

### Where the ~900 µs stands and what could remain

| Component | µs/q | Reducible? |
|---|---:|---|
| ANN: distance math | ~150 | No (SIMD'd, call count pinned by recall gate) |
| ANN: traversal bookkeeping | ~455 | Partially — visited-set/heap micro-opts, but hashing already proven a wash; realistic upside ≤100 µs |
| Re-rank + project (200 rows) | ~222 | Partially — decode pushdown (only key+vector cols), upside ~100 µs |
| Parse/plan/snapshot | ~74 | Plan cache already in; marginal |

Realistic floor with the remaining micro-levers: **~700–750 µs native** — at
or just above the target, for two more rounds of complexity. pgvector's 380 µs
at this recall implies graph/quantization work (PQ, smaller ef via better
graph quality), which is a different item.

### Open question (needs sign-off — §0.6 rule 6)

Either (a) revise acceptance to ≤1 ms native/Docker at 10k (achieved: ~900 µs,
2.6× total improvement this item, 26× from the pre-item-72 2.38 ms→900 µs
path), and file the remaining ~200 µs of micro-levers + PQ/graph-quality as a
new item; or (b) keep ≤700 µs open and implement re-rank decode-pushdown +
traversal micro-opts next session. Docker/Linux confirmation run pending
either way (W2 rung no-regression check folds into the consolidated bench).
