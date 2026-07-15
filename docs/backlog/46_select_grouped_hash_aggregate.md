# SELECT grouped remaining gap: full-row decode on GROUP BY + no vectorised hash-aggregate

**Type:** Performance
**Status:** SHIPPED — PR #117 (2026-07-15); see `PROGRESS.md` for bench numbers
**Priority:** Medium — GROUP BY g at 40k rows is 0.60× PG (+67%); after the read-path items (#45, #44) the aggregation path will be the next visible gap.

---

## Measured gap (2026-07-15, MM_CRUD_ROWS=20000, matched fsync)

| engine | rec/s | ratio |
|---|---:|---|
| unidb | 4 947 561 | 0.60× |
| Postgres | 8 239 353 | — |

`cols/row = 4.00`, `dec/row = 1.00` — every one of 40k rows is fully decoded even though the query only needs column `g` (column index 2 of 4) for grouping.

## Root causes

### 1. Full-row decode for GROUP BY — one unreferenced TEXT column decoded per row
`exec_select` calls `decode_row` (or `deform_row` + `project_row`) which materialises all 4 columns per row regardless of the GROUP BY expression. The `body TEXT` column is ~40 bytes per row and allocates a `String` per row — 40k `String` allocs that are immediately discarded.

The B2 decode-pushdown already strips unreferenced columns for plain SELECT (`deform_row` decodes only needed columns). That same pushdown does **not** yet apply on the aggregation path: `exec_aggregate_grouped` (or whichever route handles GROUP BY) calls full `decode_row`.

**Fix:** extend the decode-pushdown into the aggregate path. The only column needed for `SELECT COUNT(*) GROUP BY g` is `g`; the pushdown column mask should include only the GROUP BY exprs and any aggregated columns, not all columns.

### 2. Row-at-a-time hash-aggregate with boxed Literal keys
Grouping uses a `HashMap<Vec<Literal>, AggState>` (or equivalent). Each key is a heap-allocated `Vec<Literal>`; each `Literal::Text` or `Literal::Bytes` value adds a `String` heap allocation. For `g INT`, the Literal is `Literal::Int(i64)`, which avoids the String cost — but the `Vec` wrapper still allocates.

**Fix:** for integer-typed GROUP BY keys, use a `HashMap<i64, AggState>` directly (no Vec, no Literal boxing). A single-key integer grouping is the common case and can be specialised without a full vectorised aggregate engine.

### 3. No partial-aggregate parallelism
The parallel scan path (`parallel_resolve_candidates`) collects candidates then applies the filter/project in workers, but aggregation is always serial — all matched rows are aggregated by a single thread after all workers return. Postgres's parallel hash-aggregate (each worker maintains a local hash table, final merge at gather) avoids this serialisation point.

**Fix:** post-filter, each worker could maintain a local `HashMap<GroupKey, AggState>` and return it; the coordinator merges partial results (one pass over at most `degree` maps). Safe because `COUNT(*)` / `SUM` / `MAX` all support partial aggregation.

## Acceptance criteria

- `SELECT COUNT(*) GROUP BY g` on a 40k-row table with 5 distinct values of `g`: unidb ≥ 7 M rec/s (≈ PG level or better).
- `cols/row` counter drops from 4.00 to 1.00 for a GROUP-BY-only query (proof of pushdown).
- All existing GROUP BY correctness tests remain green.
- `PROGRESS.md` records before/after with absolute numbers.

## Depends on / builds on

- B2 decode-pushdown (`src/sql/executor.rs` — `deform_row`, `project_row`, column mask) — extend mask derivation to include aggregate path.
- `src/sql/executor.rs` — `exec_aggregate_grouped` (or the equivalent aggregation dispatch).
- Item 45 (`45_select_filtered_parallel_btree_scan.md`) — thread-pool + B-tree partition lay groundwork for partial-aggregate parallelism.
