# SELECT JOIN: hash join for equi-joins

**Type:** Performance
**Status:** NOT STARTED

## Measured gap (`030325`, Docker Linux fsync, 2026-07-16)

| operation | records | unidb (rec/s) | PG (rec/s) | ratio |
|-----------|--------:|-------------:|----------:|:-----:|
| SELECT JOIN orders/customers | 10000 | 683,722 | 2,396,118 | **0.29×** |

Platform: aarch64 · 18 cores · Linux 6.12.76-linuxkit. Both engines `fsync` (matched durability).

Estimated after hash join: **0.70–1.00×** PG (+140–245%). The gain is algorithmic — it does not depend on architecture or platform.

## Root cause

The executor uses a **nested-loop join without an inner-side index**. For `SELECT … FROM orders JOIN customers ON orders.customer_id = customers.id`:

1. For each of the 10k orders, the executor scans the 20k customers table to find the matching row.
2. Total work: O(n × m) = O(10k × 20k) = 200M comparisons per query.

Postgres uses a **hash join**: build a hash table over the smaller relation (customers, 20k rows → one-time O(m) pass), then probe it once per row in the outer relation (orders, 10k rows → O(n) probes). Total: O(n + m) = O(30k). At these sizes the algorithmic difference is the dominant factor.

The customers table has a B-tree on its PK (`id`) via item 35's implicit unique-enforcement index. An **index-nested-loop** join (probe the customers B-tree per order row) is a lower-effort alternative: O(n log m) = O(10k × 15) ≈ 150k comparisons — still much better than O(n × m) = 200M.

**Implementation priority:** index-nested-loop first (wire up the existing `customers.id` unique index on the inner side of the join), then hash join as a follow-on once the join planner exists. Index-nested-loop alone should reach ~0.50–0.70× PG; hash join should reach 0.70–1.00×.

## Plan

### Phase A — Index-nested-loop join
When the inner relation has a unique/PK index on the join column, rewrite the executor's inner-loop pass to use a B-tree point-lookup instead of a heap scan.

- `src/sql/executor.rs` — `exec_select` join branch: detect equi-join predicate `a.col = b.col` where `b.col` has a unique index. For each outer row, call `btree.get_exact(join_key)` instead of iterating `b` rows.
- No planner changes required for Phase A — hard-wire the index detection in the executor for the equi-join pattern.
- Crash harness: no WAL change, no format change. Tests: join correctness with duplicate keys, NULL join columns, empty inner relation.

### Phase B — Hash join
Build a hash table over the smaller relation at query start, probe for each outer row.

- Allocate `HashMap<Literal, Vec<Row>>` over the inner relation before the outer loop.
- Memory budget: cap the in-memory hash table; fall back to nested-loop when the inner relation exceeds the budget (spill-to-disk is out of scope).
- Planner cost model: choose hash join when inner relation fits in budget and no index exists; prefer index-nested-loop when an index is available.

## Acceptance criteria

- Phase A: `SELECT JOIN` reaches ≥ 0.50× PG at 10k rows with the `customers.id` unique index.
- Phase B: `SELECT JOIN` reaches ≥ 0.70× PG at 10k rows (or higher with hash join).
- All existing join tests pass; NULL-safe join semantics verified.
- `PROGRESS.md` records before/after with the Table 5 `SELECT JOIN` row.

## Depends on / builds on

- `src/sql/executor.rs` — join path.
- Item 35 (`35_unique_constraint_full_scan.md`) — SHIPPED; unique-enforcement B-tree on PK is the index that Phase A probes.
- Item 36 (`36_foreign_key_row_enforcement.md`) — SHIPPED; FK structure implies the join pattern benchmarked in Table 5.

## Why highest priority

The measured gap (0.29× PG) is entirely algorithmic — O(n × m) vs O(n + m). No other item offers a comparable gap-closure opportunity without touching locked decisions. Index-nested-loop (Phase A) requires only wiring up an existing B-tree lookup; hash join (Phase B) is a self-contained allocator addition. Neither touches WAL, the buffer pool, or MVCC.
