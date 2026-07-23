# 106 — Vector query pgvector-class tier (≤400 µs): quantization / graph quality

**Type:** Performance
**Status:** 🔄 IN PROGRESS — Step-0 complete 2026-07-22 (PR #204) —
recall-vs-ef curve measured AND a new dominant lever found: the upper-layer
descent is fully uncached (~290 µs ef-independent, invisible to the old
counters). Lever ordering revised — see the Step-0 section at the end.

## Problem

After item 92 (levers 1–3, 5, 7), warm NEAR at 10k×dim128 is **~900 µs**
(stable ±2 µs) vs pgvector's **380 µs** at matched recall. The measured phase
split (Q_ANN_NANOS / Q_RERANK_NANOS, permanent instrumentation) says where
the remaining time is and why micro-optimization is exhausted:

| Component | µs/q | Why micro-levers are done |
|---|---:|---|
| ANN: distance math | ~150 | SIMD'd (8-lane); call count (3,748/q) pinned by recall gate |
| ANN: traversal bookkeeping | ~455 | Hashing proven a wash (item 92 Lever 6 A/B); 5 MiB random-access working set is TLB/cache-bound regardless of layout (Lever 7 evidence) |
| Re-rank + project (ef=200 rows) | ~222 | Decode-pushdown worth ~100 µs, but bounded |
| Parse/plan/snapshot | ~74 | Plan cache already in |

Recall@10 at 10k is exactly 0.900 (the gate), so `ef_search` cannot be
reduced. Closing the remaining 2.4× requires **fewer or cheaper distance
computations at equal recall** — an algorithmic change, not tuning.

## Candidate levers (rank by measured ROI before building — §0.6)

1. **Graph quality (heuristic neighbour selection at build).** Current build
   may use simple nearest-M selection; HNSW's `select_neighbors_heuristic`
   (diversity pruning, keep-pruned-connections) typically yields equal recall
   at ~half the ef → directly halves distance calls AND traversal. Cheapest
   to try: build-time only, no query-path or format change.
2. **Scalar quantization (SQ8) for the ANN phase.** int8 vectors in the slab
   (4× smaller working set: 5 MiB → 1.25 MiB, fits L2) with f32 re-rank from
   the heap (already exists as the exact re-rank). Recall impact must be
   measured against the 0.90 gate. `VecArena` (item 92 Lever 7) is the
   natural home — add a parallel `data_q: Vec<i8>` + scale factors.
3. **Product quantization (PQ)** — bigger win, bigger complexity; only if
   SQ8 + graph quality fall short.
4. **Re-rank decode-pushdown** (~100 µs) — decode only key + vector columns
   for candidate rows instead of full `decode_row`. Independent of 1–3;
   fold in opportunistically.

## Targets

- Warm NEAR at 10k×dim128, recall@10 ≥ 0.90: **≤ 400 µs** native
  (pgvector parity band; pgvector = 380 µs at matched recall).
- No write-path regression: HNSW insert throughput and W2 ladder rung within
  noise of the pre-item baseline.
- Fairness rule unchanged: competitor numbers quoted at matched recall.

## Acceptance criteria

- [ ] Step-0: measure recall-vs-ef curve for the current graph BEFORE any
      lever (is the graph the bottleneck, or the vectors?). This decides
      lever 1 vs lever 2 ordering — do not skip it.
- [ ] Chosen lever(s) implemented; recall@10 ≥ 0.90 at 10k maintained.
- [ ] Warm NEAR ≤ 400 µs at 10k native, confirmed on Linux/Docker.
- [ ] Crash harness green (index build path touched ⇒ P60-series re-run).
- [ ] Full suite + clippy + fmt clean; PROGRESS.md metrics recorded.


## Step-0 results (2026-07-22) — curve + a lever nobody priced

### Recall-vs-ef curve, current graph, 10k×dim128 (`tests/perf_item106.rs`)

| ef | recall@10 | warm µs/q |
|---:|---:|---:|
| 40 | 0.640 | 425 |
| 60 | 0.715 | 410 |
| 80 | 0.805 | 494 |
| **120** | **0.910** | **601** |
| 160 | 0.925 | 701 |
| 200 (default) | 0.945 | 820 |
| 300 | 0.960 | 1074 |

The curve is **steep** — 0.90 needs ef≈120 on the current graph → L1 (build-
time heuristic neighbour selection) is justified, target 0.90+ at ef≤80 with
real margin (today's 0.910@120 is one wobble from the gate).

### The floor: ~410 µs at ef=40 — and where it lives

Phase split at ef=40 (`UNIDB_HNSW_EF_SEARCH=40`, perf_item92 timers):
**ANN 321 µs** · re-rank 47 µs · other ~67 µs. Re-rank scales with ef as
modeled; **ANN has a ~290 µs ef-independent component.**

Code-confirmed root cause: the **upper-layer descent is completely
uncached** — `get_upper_nbrs` runs a `DiskBTree::search_eq` per hop, and the
upper `search_layer` calls pass ALL caches as `None`, so every neighbour
evaluation pays `find_node_loc` (another B-tree walk) + an uncached node
load. ~4 layers × ~16 neighbours ≈ the measured ~290 µs. These fetches don't
increment `Q_DISK_FETCHES` (which counts only the layer-0 miss branch) —
which is why the "0 disk fetches, cache is working" line was true-but-
incomplete since item 72.

### Revised lever ordering (replaces the filed 1–4)

1. **L0′ — upper-layer cache** (NEW, first): items-72/73 pattern applied to
   layers >0 — hundreds of nodes, trivial memory, prefetchable at CREATE
   INDEX/open. Expected −250–290 µs at every ef. Cheapest, biggest.
2. **L1 — graph-quality heuristic selection** at build: target 0.90+ at
   ef≤80 (halves distance calls + traversal). Interacts with item 107
   (heavier insert → worker throughput/drain must be measured, not hidden).
3. **L3 — re-rank decode-pushdown**: smaller once ef drops (47 µs at ef=40)
   but still worth ~50–100 µs at ef≈80.
4. **L2 — SQ8 slab quantization: demoted to reserve** — only if 1–3 land
   short of 400 µs.

**Projection with 1+2(+3): ~280–330 µs — under the 400 µs target with
margin, possibly without quantization at all.**

### Shipped in Step-0

`ef_search` is now runtime-tunable (`UNIDB_HNSW_EF_SEARCH` env +
`set_ef_search()`, default unchanged at 200) — needed for the sweep, doubles
as the ops knob the eventual retune requires. Probe test
`tests/perf_item106.rs` (curve is re-runnable after every lever).


## Unit 1 result (2026-07-22) — shipped, and the model corrected

**Measured: uniform −55–60 µs at every ef** (ef=120 gate point 601 → **549 µs
@ 0.910**; ef=200 820 → 774; **cold 2,331 → 1,042 µs** from the upper-list
prefetch). Implementation: upper lists share `HnswL0Cache`'s arena via the
naturally-disjoint layer-key space (`encode_upper_cache_key`, layer≥1 ⇒
≥2^48) — zero new structures, COW/generation/merge inherited; descent now
threads both caches; prefetch pre-loads the upper tree; `Q_UPPER_CACHE_HITS`
counter added.

**Attribution correction (honest):** Step-0 projected −250–290 µs here; the
true upper-layer cost was ~60 µs. The error: distance calls scale SUBLINEARLY
with ef (1,193 at ef=40 vs 3,748 at ef=200 — not ef-proportional), so the
ef=40 "floor" I attributed to upper layers was mostly **per-neighbour
bookkeeping: ~140 ns/call** (VecArena HashMap get + `visited` HashSet +
BinaryHeap) on top of ~40 ns of SIMD distance. That bookkeeping is the real
remaining ANN wall (~180 ns × calls fits both ef points). Corollary: item
92's "hasher is a wash" A/B ran in the ±120 µs clone-variance era and only
tested a hasher swap — the structural fix (below) is different and now
evidence-motivated.

### Revised units (post-Unit-1 model: warm ≈ 180 ns×calls + re-rank + 74 µs)

- **Unit 2a — dense-slot bookkeeping:** resolve rid→arena-slot ONCE per
  neighbour; `visited` becomes a slot bitset (no SipHash), vector access a
  direct slab index (no HashMap get). Est. −150–200 µs at ef=120.
- **Unit 2b — L1 graph quality** (unchanged): 0.90+ at ef≤80 for margin.
- **Unit 3 — L3 decode-pushdown + ef retune + certification.**
  Projection at ef=120 after 2a+3: **~285–330 µs @ 0.910** — under target
  even before 2b's margin.
