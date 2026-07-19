**Type:** Performance
**Status:** ⏳ NOT STARTED

# Item 96 — Query plan cache: parse + compile once per unique SQL

## Problem

Every `/sql` call re-parses and re-plans the query from scratch.
`compare.py` runs 9 benchmark queries — each identical across runs — and every
call pays full parse + bind + plan cost. Measured in the demo: COUNT queries
at 1k rows take 1.4–2.0 ms via REST while Postgres (psycopg2, cached plan)
takes 0.3–0.7 ms.

SQL parse + plan overhead estimate per query:
- `sqlparser` tokenise + AST build: ~50–200 µs
- Logical → physical plan selection (index vs seq-scan decision): ~20–100 µs
- **Total avoidable overhead per repeated query: ~100–300 µs**

Postgres avoids this for prepared statements and caches generic plans after 5
executions. Every psycopg2 `cursor.execute()` benefits from PG's plan cache.
unidb has no equivalent.

## What to build

### 1. Plan cache keyed by (sql_hash, schema_epoch)

```rust
struct PlanCache {
    /// LRU map: (u64 sql_hash, u64 schema_epoch) → Arc<PhysicalPlan>
    entries: LruCache<(u64, u64), Arc<PhysicalPlan>>,
    capacity: usize,  // default 1_024 entries, UNIDB_PLAN_CACHE_SIZE env
}
```

- **sql_hash**: FxHash64 of the raw SQL string (fast, non-crypto).
- **schema_epoch**: a monotonic counter incremented on every DDL
  (`CREATE/DROP TABLE`, `CREATE/DROP INDEX`, `ALTER TABLE`, `GRANT`,
  `CREATE POLICY`). A DDL invalidates all cached plans.
- Cache is per-engine instance (not per-connection). Shared with an `RwLock`.

### 2. Cache lookup in `execute_sql`

```
execute_sql(sql):
  hash = fx_hash(sql)
  if let Some(plan) = plan_cache.get((hash, schema_epoch)):
      return run_plan(plan, txn)   // skip parse + plan
  plan = parse(sql) → logical_plan → physical_plan
  plan_cache.insert((hash, schema_epoch), plan.clone())
  run_plan(plan, txn)
```

### 3. Invalidation triggers

- Any DDL statement clears `schema_epoch += 1` (all cached plans stale).
- DML (INSERT/UPDATE/DELETE) does NOT invalidate the cache.
- `TRUNCATE` is DDL → invalidates.
- No per-table invalidation needed — schema_epoch is global (simple and safe;
  DDL is rare in the demo workload).

### 4. Parameterised plans (follow-on)

Literal values in WHERE (`WHERE status = 'delivered'`) bake the value into the
plan. An identical query with a different literal is a cache miss. For the
compare.py workload (fixed query text) this is fine. Parameterised binding
(`WHERE status = $1`) is a follow-on item.

## Targets

- Repeated `SELECT COUNT(*) FROM customers` (no WHERE): **< 0.1 ms engine
  time** on plan-cache hit (no re-parse cost).
- compare.py COUNT queries: reduce unidb engine latency by ~100–300 µs each.
- `cargo bench` plan-cache-hit latency vs cold parse: ≥ 3× speedup on the
  parse-only microbench.
- No correctness regression: cached plan must never run against a stale schema
  (schema_epoch gate).

## Acceptance criteria

- Unit test: same SQL called twice returns identical results; second call
  exercises the plan-cache hit path (add a counter + assert).
- DDL invalidation test: `CREATE TABLE t2`, then `SELECT * FROM t2` hits a
  miss (not the old cached `t2` plan).
- Load test: 1000 concurrent `/sql` calls with the same query; no deadlock
  on the `RwLock`; plan compiled exactly once (assert counter = 1).
- compare.py COUNT queries: each ≤ 1.0 ms unidb (down from 1.4–2.0 ms).

## ROI

- Affects every repeated query through the REST API — the primary access path
  for the Studio demo and any HTTP client.
- Compare.py runs each query once (no warm-up), so the first-call cost applies
  every time. Repeated client calls (Refresh button in Studio) benefit fully.
- Low implementation risk: cache is additive, fallback to re-parse on miss.
