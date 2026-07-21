# 106 — Vector query pgvector-class tier (≤400 µs): quantization / graph quality

**Type:** Performance
**Status:** ⏳ NOT STARTED — filed 2026-07-21 when item 92's acceptance was
revised to ≤1 ms with user sign-off (see `92_vector_query_next_tier.md`)

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
