**Type:** Performance
**Status:** ⏳ NOT STARTED — follow-up to shipped items 72 + 73

# Item 92 — Vector query next tier: 2.38 ms → pgvector-class (≤700 µs)

## Problem

After items 72 (L0 neighbour cache) + 73 (vector hot cache), warm NEAR at 10k
vectors is **2.38 ms** — 11× better than a week ago but still **~6× behind
pgvector (380 µs)** and ~19× behind ffsdb (126 µs). The "strip the SQL
transaction" hypothesis is contradicted by the comparison itself: pgvector
runs a *full* SQL/MVCC transaction per query and is 6× faster — txn setup is
tens of µs; the residual ~2 ms is per-hop node access cost.

## Step 0 — measure before building (§0.6)

Instrument one warm NEAR query at 10k: report
`(node fetches) × (cost per fetch)` split by layer (L0-cache hit / vector-
cache hit / pool fetch), plus txn setup time. Prediction to falsify:
fetch-count × fetch-cost ≈ 2 ms, txn overhead < 100 µs. Route the sprint by
what this shows.

## Candidate levers (rank after Step 0)

1. **Item 86 synergy**: every pool fetch the beam search still performs pays
   the 8 KiB clone + CRC verify — lands for free once item 86 ships.
2. **Zero-copy node layout**: serve cached L0 neighbours/vectors as slices
   into a contiguous arena instead of per-node `Vec` clones; kills per-hop
   allocation.
3. **SIMD distance** (dim=128 f32): NEON/AVX2 via `std::simd` or hand-rolled;
   scalar dot product at ef_search=200 × M=16 hops is real CPU.
4. Read-only NEAR fast path (skip snapshot allocation for single-statement
   reads) — worth tens of µs; do last.

## Targets

- Warm NEAR at 10k, recall@10 ≥ 0.90 (ef_search=200 unchanged):
  **≤ 700 µs** (pgvector-class). The ffsdb 126 µs tier requires the arena
  layout everywhere and is a later, separate decision.
- **Fairness rule for the comparison table**: pgvector/ffsdb latencies must be
  quoted at matched recall — pin recall@10 next to every latency number.

## Acceptance criteria

- Step-0 profile table committed to the report before any lever is built.
- ≤ 700 µs warm at 10k with recall gate held; Docker bench confirms on Linux;
  no regression in W2 (vector-write) rung of the decompose ladder.
