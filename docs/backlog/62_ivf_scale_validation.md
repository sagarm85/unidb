**Type:** Performance
**Status:** 🔄 IN PROGRESS (branch `62-ivf-scale-validation`) — bench run complete; PR pending

## Summary

Empirically measure where the current IVF-Flat ANN index (`DiskIvfIndex` in
`src/disk_vector.rs`) breaks down: track NEAR query latency, candidate count,
and recall@10 at 1k/10k/100k/1M rows with 128-dim vectors. This is the gate
that justifies the disk-HNSW implementation effort (item 61).

## Critical bug found in W2 bench (confirmed 2026-07-17)

The existing W2 ladder point (`mm_ladder_point` in `benches/decompose.rs`)
creates the IVF index on an **empty** table before inserting rows:

```rust
engine.execute_sql(setup, "CREATE INDEX iv ON t USING HNSW (embedding)").unwrap();
// ... then inserts rows ...
```

`ivf_params(0)` (called from `exec_create_index` at `executor.rs:46-51`) trains
on an empty sample → `nlist = 1` (single origin centroid) → every NEAR query
fetches ALL rows as candidates → brute-force linear scan at O(N).

This means:
- All previous W2 NEAR benchmarks measured brute-force, not IVF approximation.
- At 100k rows with `nlist=1`: every NEAR query fetches all 100k candidates.
- The W2 write-path cost (IVF insert: assign vector → centroid → B-tree) was
  still measured correctly because the index *exists*; only the query path was
  degenerate.

## What this item measures

### A — Fix the nlist=1 bench artifact

Add `bench_ivf_scale_validation()` in `benches/decompose.rs` (activated by
`UNIDB_BENCH=ivf_validate`). The bench:

1. Inserts N rows with 128-dim random float32 vectors (fixed seed, reproducible).
2. Creates the IVF index **AFTER** all rows are inserted → correct `nlist = √N`
   (capped at 256).
3. Runs 100 NEAR queries and measures: latency (cold + warm cache), recall@10.

### B — Recall@10 measurement

For each corpus size (1k, 10k, 100k — 1M skipped, extrapolated):

1. IVF top-10 via `SELECT id FROM t WHERE NEAR(embedding, [...], 10)`.
2. Brute-force ground truth: exact L2 over in-RAM corpus vectors.
3. recall@10 = |IVF_top10 ∩ BF_top10| / 10, averaged over 100 queries.

### C — Expected findings

| corpus size | nlist (actual) | nprobe | est. candidates | nprobe/nlist |
|---|---|---|---|---|
| 1k | 32 | 8 | 250 | 25% |
| 10k | 100 | 12 | 1 200 | 12% |
| 100k | 317 → 256 | 32 | 12 500 | 12.5% |
| 1M | 1000 → 256 | 32 | 125 000 | 3.2% |

The `nlist ≤ 256` cap means the 1M-row index probes only 3.2% of the space →
recall@10 is expected to fall below 0.80. This is the hard evidence for item 61.

### D — Latency

Cold-cache: first NEAR query after index creation (posting-list pages cold).
Warm-cache: average of last 50 queries after 50-query buffer-pool warm-up.

### E — Tests

- `tests/ivf_scale_validation.rs::recall_at_k_computation_correct` — pure
  math: |intersection|/k formula is correct.
- `tests/ivf_scale_validation.rs::nlist_correct_when_index_created_after_insert`
  — observational: a NEAR k=400 query on a 400-row pre-populated-index table
  returns < 400 rows (proving nlist > 1, not brute-force).

## Results

See `PROGRESS.md` — Item 62 entry (added after bench run).

## IVF-Flat report table (Mac M5 Pro arm64, 2026-07-17)

`MM_SIZES=1000,10000,100000 UNIDB_BENCH=ivf_validate cargo bench --bench decompose`

| corpus size | nlist (actual) | nprobe | candidates | NEAR latency (cold) | NEAR latency (warm) | recall@10 |
|---|---|---|---|---|---|---|
| 1k | 32 | 8 | 250 | 1.04 ms | 0.77 ms | 0.690 |
| 10k | 100 | 12 | 1 200 | 1.94 ms | 1.73 ms | 0.378 |
| 100k | 256 (capped) | 32 | 12 500 | 35.73 ms | 17.04 ms | 0.421 |
| 1M | 256 (capped) | 32 | ~125 000 | not measured | not measured | extrapolated |

## Files changed

- `benches/decompose.rs` — `bench_ivf_scale_validation()` function + `ivf_validate`
  match arm in `main()`.
- `tests/ivf_scale_validation.rs` — two tests above.
- `docs/backlog/61_disk_hnsw_planning.md` — new (disk-HNSW item gated on this).
- `docs/backlog/62_ivf_scale_validation.md` — this file.
- `docs/backlog/backlog_index.md` — items 61, 62 registered; "Next new file →
  `63_…`" updated.
- `PROGRESS.md` — Item 62 entry with bench results.
- `MEMORY.md` — current state updated.
