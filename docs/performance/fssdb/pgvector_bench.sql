-- pgvector HNSW comparison, replicating FFS's eval setup:
-- 10,000 vectors, dim 128, HNSW m=16, ef_construction=200, k=10, 200 queries.
\timing off
DROP TABLE IF EXISTS items;
CREATE EXTENSION IF NOT EXISTS vector;
CREATE TABLE items (id serial PRIMARY KEY, embedding vector(128));

-- Insert 10,000 random 128-dim vectors.
INSERT INTO items (embedding)
SELECT ARRAY(SELECT random()::real FROM generate_series(1,128))::vector
FROM generate_series(1,10000);

-- Measure HNSW index build time (m=16, ef_construction=200).
\echo '=== HNSW index build (m=16, ef_construction=200) ==='
\timing on
CREATE INDEX items_hnsw ON items USING hnsw (embedding vector_l2_ops) WITH (m=16, ef_construction=200);
\timing off

SET hnsw.ef_search = 40;
ANALYZE items;

-- Query latency: 200 queries, k=10, mean over the batch (probe vectors are
-- pre-fetched into an array so only the ANN search is timed).
\echo '=== HNSW query latency (k=10), mean over 200 queries ==='
DO $$
DECLARE
  probes vector(128)[];
  t0 timestamptz; t1 timestamptz; i int;
BEGIN
  SELECT array_agg(embedding) INTO probes
    FROM items WHERE id <= 200;
  -- warm-up
  FOR i IN 1..200 LOOP
    PERFORM id FROM items ORDER BY embedding <-> probes[i] LIMIT 10;
  END LOOP;
  t0 := clock_timestamp();
  FOR i IN 1..200 LOOP
    PERFORM id FROM items ORDER BY embedding <-> probes[i] LIMIT 10;
  END LOOP;
  t1 := clock_timestamp();
  RAISE NOTICE 'pgvector HNSW mean query latency: % us/query',
    round((extract(epoch from (t1-t0))*1e6/200)::numeric, 1);
END $$;

-- No-index brute-force query latency for reference.
\echo '=== brute-force (no index) query latency (k=10), mean over 200 queries ==='
DROP INDEX items_hnsw;
DO $$
DECLARE
  probes vector(128)[];
  t0 timestamptz; t1 timestamptz; i int;
BEGIN
  SELECT array_agg(embedding) INTO probes FROM items WHERE id <= 200;
  t0 := clock_timestamp();
  FOR i IN 1..200 LOOP
    PERFORM id FROM items ORDER BY embedding <-> probes[i] LIMIT 10;
  END LOOP;
  t1 := clock_timestamp();
  RAISE NOTICE 'pgvector brute-force mean query latency: % us/query',
    round((extract(epoch from (t1-t0))*1e6/200)::numeric, 1);
END $$;

DROP TABLE items;
