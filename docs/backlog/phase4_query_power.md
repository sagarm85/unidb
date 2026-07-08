# Phase 4 — Query power (SQL lane)

## Status as of 2026-07-09: DONE (all checkpoints P4.a–P4.e shipped on branch `query-power`; see `PROGRESS.md`'s Phase 4 entry).

Real SQL + a query brain. Companion to [`roadmap.md`](roadmap.md) §4. Runs in
the **SQL lane after Phase 2** (types), and benefits from **Phase 3** durable
indexes (index-nested-loop, statistics). Parallel with the Core lanes; owns
`catalog.rs` + `sql/*`.

## Context

Today there are **no joins, no aggregates, no ORDER BY/GROUP BY, no subqueries,
and no optimizer** — single-table filter/project only. This phase makes unidb a
real query engine. The hard, central piece is the **cost-based optimizer** — the
classic "sink" that determines whether joins are fast or catastrophic.

## Scope

- **IN:** joins, aggregates + grouping + sort, subqueries/CTEs, statistics +
  cost-based optimizer, `EXPLAIN`.
- **OUT:** window functions + recursive CTEs (follow-up); columnar/vectorized
  execution (parked Track E); intra-query parallelism (needs Phase 5).

## Checkpoints

### P4.a — Join operators
- **Nested-loop** (only when the inner side is indexed), **hash join** (equi-
  joins; build + probe; **spill-to-disk** when the build side exceeds the memory
  budget), **merge join** (sorted/indexed inputs, range joins).
- Files: `sql/executor.rs` (join nodes), `sql/logical.rs` (join plan),
  `sql/parser.rs` (sqlparser already yields `JOIN`).
- Tests: join correctness vs. a reference (inner/left/right); hash-join spill;
  3+-table joins.

### P4.b — Aggregates + grouping + sort
- `COUNT`/`SUM`/`AVG`/`MIN`/`MAX`, `GROUP BY` (hash aggregation), `HAVING`,
  `ORDER BY` (external/merge sort when large), `DISTINCT`, `LIMIT`/`OFFSET`.
- Files: `sql/executor.rs`, `sql/logical.rs`, `sql/parser.rs`.
- Tests: correctness vs. reference; large `ORDER BY` spills; grouping on skewed
  keys.

### P4.c — Subqueries + CTEs
- Scalar / `IN` / `EXISTS` subqueries (correlated + uncorrelated); `WITH` CTEs.
- Tests: correlated subquery correctness; CTE reuse.

### P4.d — Statistics + cost-based optimizer (the central, hardest piece)
- **`ANALYZE`** collects per-table statistics: row counts, distinct-value counts,
  histograms — persisted on the catalog.
- A **cost model** + **join-order search** (dynamic programming for ≤ ~10
  tables, greedy beyond) choosing join order, join algorithm, and index-vs-scan.
- Files: new `sql/optimizer` module, `catalog.rs` (stats storage),
  `sql/executor.rs`.
- Tests: optimizer picks the right plan on skewed/selective data; join-order
  correctness; index-vs-scan crossover.

### P4.e — EXPLAIN / EXPLAIN ANALYZE
- Expose the chosen plan tree with **estimated** costs/rows, and (with `ANALYZE`)
  **actual** rows/time — the essential production diagnostic.
- Files: new `sql/explain`, `sql/executor.rs`, `server/*` route,
  `docs/REST_API.md`.
- Tests: plan shape matches expectation; `ANALYZE` actuals populated.

## Locked decisions touched

- **None reversed.** This is CLAUDE.md §1's "practical subset" growing — record
  the deliberate scope (these features, not full ANSI SQL) in `PROGRESS.md`, as
  §3 requires for scope decisions.
- Catalog gains statistics storage (additive).

## Verification gates (Phase 4 done =)

- Join / aggregate / subquery correctness vs. a reference DB on a shared test
  set.
- Optimizer picks correct plans on skewed data; no O(n·m) nested-loop where a
  hash join is available.
- `EXPLAIN` accurate; a TPC-H-subset benchmark recorded.
- `clippy -D warnings` + `fmt` clean; one PR per checkpoint; rebase onto
  `origin/main` before each PR; `PROGRESS.md`/`MEMORY.md` updated.

## Known limitations / deferred

- No window functions / recursive CTEs in v1.
- No columnar/vectorized execution (parked) — the executor stays row/batch;
  large analytical scans are "good enough," not OLAP-class.
- No parallel query execution — depends on Phase 5 concurrency.
- The optimizer starts simple (few rules + histograms) and improves; it is not
  a from-day-one world-class planner.
