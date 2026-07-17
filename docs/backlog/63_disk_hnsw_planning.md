**Type:** Performance
**Status:** NOT STARTED — gated on item 62 (IVF scale validation) results

## Summary

Design and implement an on-disk HNSW index to replace the current IVF-Flat
index for high-recall approximate nearest-neighbor search at corpus sizes ≥ 100k
rows, where the IVF-Flat approach (`nlist` capped at 256, `nprobe` = 32 →
3.2% scan at 1M rows) degrades to recall@10 < 0.80.

## Why this is gated on item 62

Item 62 (`62_ivf_scale_validation.md`) empirically measures IVF-Flat recall,
latency, and candidate count at 1k/10k/100k/1M rows. Its results determine:

1. **At what scale** IVF-Flat recall drops below the production acceptance gate
   (recall@10 ≥ 0.90).
2. **Whether the IVF cap (`nlist ≤ 256`) is the only problem** (tunable without
   an architectural change) or whether the index structure itself needs replacing.
3. **The latency target** for disk-HNSW (it must beat IVF-Flat at the break-even
   corpus size to justify the implementation complexity).

Do not start this item until item 62 results are recorded in `PROGRESS.md`.

## Expected scope (to be revised once item 62 ships)

- On-disk HNSW graph stored as adjacency pages (similar to edge-list index in
  `graph.rs`) with WAL-logged mutations.
- O(log N) search instead of IVF-Flat's O(nprobe × N/nlist) scan-of-cells.
- Async background build (like the retired in-RAM HNSW worker) to avoid blocking
  the NEAR query path during initial construction.
- No FORMAT_VERSION bump required if implemented as a new index kind tag
  (`IndexKind::HnswDisk`).

## Acceptance criteria (placeholder — fill in after item 62)

- recall@10 ≥ 0.95 at 1M rows (128-dim Euclidean).
- NEAR latency at 1M rows: < 10 ms warm-cache (to be compared against IVF-Flat
  item 62 warm-latency measurement).
- No regression on existing IVF-Flat recall/latency at 10k rows.
- Crash harness: index survives kill during HNSW graph page write.
