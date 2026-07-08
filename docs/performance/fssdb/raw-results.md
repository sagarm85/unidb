# Raw benchmark logs — unidb vs FFSDB evaluation

Provenance for every unidb number in [`README.md`](./README.md). All runs
**2026-07-08**, Apple **M5 Pro** / 48 GB / macOS (Darwin 25.4) / Rust
1.95.0 / Postgres 18.4 + pgvector 0.8.4, release builds.

The FFS numbers in `README.md` are reproduced verbatim from
<https://ffsdb.com/evals> (FFS `2.0.0-alpha.1`); they were **not** re-run
here (FFS is a separate codebase not present in this repo).

---

## `cargo bench --bench graph` (median of criterion's 3-point estimate)

```
adjacency_scan n=1000: 8 distinct pages
  naive/1000    time: [874.38 µs 876.21 µs 877.52 µs]
  batched/1000  time: [75.135 µs 75.350 µs 75.697 µs]
  csr/1000      time: [75.319 µs 75.501 µs 75.649 µs]
adjacency_scan n=10000: 78 distinct pages
  naive/10000   time: [8.7728 ms 8.7811 ms 8.7925 ms]
  batched/10000 time: [741.68 µs 744.18 µs 747.74 µs]
  csr/10000     time: [745.93 µs 749.64 µs 754.47 µs]
edge_insert/uncontended time: [337.20 ms 341.29 ms 345.16 ms]   (100 edges → 3.41 ms/edge)
```

## `cargo bench --bench btree`

```
btree_point_select/indexed/1000    time: [3.1176 ms 3.2168 ms 3.3312 ms]
btree_point_select/full_scan/1000  time: [3.3457 ms 3.4222 ms 3.4922 ms]
btree_point_select/indexed/10000   time: [3.0451 ms 3.0667 ms 3.0885 ms]
btree_point_select/full_scan/10000 time: [4.2042 ms 4.4731 ms 4.7354 ms]
btree_range_select/indexed/1000    time: [3.0895 ms 3.1359 ms 3.1950 ms]
btree_range_select/full_scan/1000  time: [3.4210 ms 3.4566 ms 3.4888 ms]
btree_range_select/indexed/10000   time: [3.0500 ms 3.0972 ms 3.1836 ms]
btree_range_select/full_scan/10000 time: [4.8690 ms 4.9255 ms 4.9969 ms]
```

## `cargo bench --bench vector`

```
vector_insert/without_index/50   time: [242.50 ms 243.85 ms 245.27 ms]   (4.88 ms/row)
vector_insert/with_index/50      time: [233.62 ms 235.70 ms 237.88 ms]   (4.71 ms/row)
vector_insert/without_index/200  time: [893.84 ms 896.24 ms 898.68 ms]   (4.48 ms/row)
vector_insert/with_index/200     time: [2.3436 s  2.3498 s  2.3577 s ]   (11.75 ms/row)
near_query/5                     time: [3.9058 ms 3.9314 ms 3.9591 ms]
near_query/20                    time: [3.9948 ms 4.0130 ms 4.0347 ms]
near_query/50                    time: [4.0092 ms 4.0243 ms 4.0441 ms]
index_primitives/vector_index_upsert_100  time: [783.47 ms 784.10 ms 784.74 ms]  (7.84 ms/pt cumulative)
index_primitives/fulltext_search          time: [13.828 µs 13.859 µs 13.890 µs]
```

Note: criterion reports a regression on `vector_insert/*` vs a saved
baseline from an earlier M2 run on different hardware — this is criterion's
cross-run comparison, not meaningful here.

## Postgres + pgvector (fresh, replicating FFS's setup)

Script: `pgvector_bench.sql` — 10,000 random dim-128 vectors, HNSW
`m=16, ef_construction=200`, `hnsw.ef_search=40`, k=10, mean over 200
queries, timed **server-side** in `plpgsql` (no client round-trip).

```
=== HNSW index build (m=16, ef_construction=200) ===
CREATE INDEX  Time: 770.251 ms
=== HNSW query latency (k=10), mean over 200 queries ===
NOTICE: pgvector HNSW mean query latency: 43.5 us/query
=== brute-force (no index) query latency (k=10), mean over 200 queries ===
NOTICE: pgvector brute-force mean query latency: 1556.4 us/query
```

`pgvector_bench.sql`:

```sql
DROP TABLE IF EXISTS items;
CREATE EXTENSION IF NOT EXISTS vector;
CREATE TABLE items (id serial PRIMARY KEY, embedding vector(128));
INSERT INTO items (embedding)
  SELECT ARRAY(SELECT random()::real FROM generate_series(1,128))::vector
  FROM generate_series(1,10000);
CREATE INDEX items_hnsw ON items USING hnsw (embedding vector_l2_ops)
  WITH (m=16, ef_construction=200);
SET hnsw.ef_search = 40;
ANALYZE items;
-- 200 pre-fetched probe vectors, warm-up, then timed loop of
--   SELECT id FROM items ORDER BY embedding <-> probe LIMIT 10;
-- (full script in scratchpad; brute-force pass drops the index and repeats)
```

## Postgres head-to-heads reused from `PROGRESS.md`

The M2 (pgvector), M3 (adjacency table), and M4 (`SELECT … FOR UPDATE SKIP
LOCKED` queue) Postgres numbers in `README.md` §5b were **not** re-run for
this evaluation; they are cited as recorded in `PROGRESS.md`'s M2/M3/M4
entries (same-machine, server-side PG timing at the time each milestone
shipped, 2026-07-06).
