# FK UPDATE: skip constraint re-check when FK column not in SET clause

**Type:** Improvement
**Status:** NOT STARTED

## Measured gap (`030325`, Docker Linux fsync, 2026-07-16)

| operation | records | unidb (rec/s) | PG (rec/s) | ratio |
|-----------|--------:|-------------:|----------:|:-----:|
| UPDATE bulk (FK table, re-checks FK path) | 10000 | 40,423 | 734,149 | **0.06×** |

This is the worst ratio in the entire `030325` report: unidb is 17× behind Postgres on an UPDATE that touches a table with a foreign-key constraint. Non-FK UPDATE on a similarly sized table runs at 115,549 rec/s (0.14× PG). The ~3× delta between FK-UPDATE and non-FK-UPDATE exposes unconditional FK re-checking on every updated row.

Estimated after fix: **~0.12–0.18× PG** (+100–200%), matching non-FK UPDATE throughput.

## Root cause

`exec_update` calls the FK enforcement path for every updated row regardless of whether the SET clause touches the FK column (`customer_id`). For a `SET body=... WHERE ...` UPDATE on the `orders` table:

1. The FK column `customer_id` is **not** being changed.
2. The new row has the same `customer_id` as the old row.
3. Therefore the FK constraint cannot be violated — the parent reference is identical.
4. Yet unidb still performs a B-tree point-lookup on `customers.id` for every updated row to verify the reference (item 36's `enforce_fk_child_insert_update` path).

Postgres optimises this: if the FK column is not listed in the UPDATE's SET clause, the constraint trigger is not fired. unidb does not yet implement this check.

## Fix

In `src/sql/executor.rs`, `exec_update`:

1. Derive the set of columns that are actually modified by the UPDATE's SET clause (`set_cols: HashSet<ColumnIndex>`).
2. For each FK constraint on the table, check if the FK column(s) are in `set_cols`.
3. If the FK column is **not** in `set_cols`: skip `enforce_fk_child_insert_update` entirely for that row.
4. If the FK column **is** in `set_cols`: run the existing enforcement check as before.

This is a pure executor-logic change. No WAL format change, no storage change, no new crash points.

## Correctness invariant

The skip is safe because:
- INSERT-new-version MVCC copies all unchanged column values from the old row into the new version.
- The new FK column value is identical to the old FK column value.
- If the old version satisfied the FK constraint (it must have, since it was committed), the new version satisfies it identically.

Edge case: if the FK column is part of a compound SET expression that reads another column's value into the FK column (e.g. `SET customer_id = other_col`), the executor must detect `customer_id` in the output of the SET expression, not just the literal column list. The safe conservative rule: any SET expression that could write the FK column triggers the check.

## Acceptance criteria

- `UPDATE SET body=... WHERE ...` on a table with FK constraint: `unidb ÷ PG` improves from 0.06× to ≥ 0.12× on the 10k-row Table 5 workload.
- `UPDATE SET customer_id=...` (FK column IN SET clause): FK enforcement fires as before — no regression on item 36's correctness tests.
- The FK correctness proofs in the concurrency matrix (cells 23, 32) remain PASS.
- `PROGRESS.md` records before/after with absolute rec/s numbers.

## Depends on / builds on

- `src/sql/executor.rs` — `exec_update`, `enforce_fk_child_insert_update`.
- Item 36 (`36_foreign_key_row_enforcement.md`) — SHIPPED. This item optimises the hot path added by item 36; the enforcement path itself remains unchanged.

## Parallel note

This item and item 52 (Phase B decode pushdown) both edit `exec_update` but in different code sections (FK enforcement vs `matching_rows` decode). They can be developed in parallel worktrees if desired; merging is straightforward since they touch different lines.
