# Item 119 — Parent UPDATE wrongly blocked by FK RESTRICT (gate on referenced-key-in-SET)

**Type:** Improvement
**Status:** ✅ SHIPPED 2026-07-31 (branch `fix/fk-parent-update-restrict-gate`) —
found live in unidb-studio: editing `purchase_orders.shipping_address` failed with a
`FOREIGN_KEY_VIOLATION` because `po_line_items.order_id` references it. Fix +
both-direction regression test landed; full suite + crash harness green.

## Symptom

Any `UPDATE` of a parent row that has children was rejected — even when the edit
touched **no** child-referenced column:

```
UPDATE purchase_orders SET shipping_address = '1 Fairfax Blvd' WHERE id = 1
→ FOREIGN_KEY_VIOLATION: constraint violated on table 'po_line_items':
  column 'order_id' value 1 has no matching row in 'purchase_orders'
```

The message is also misleadingly phrased from the child-insert angle (that wording
is the separate cosmetic item 113); here the real defect is the check firing at all.

## Root cause

In `exec_update` (`src/sql/executor.rs`) the parent-side RESTRICT check — and its
parent phantom lock — were gated only on `has_fk_children` ("does any child
reference this table?"), **not** on whether a child-referenced column is actually
in the `SET` clause. `enforce_fk_restrict` reads the OLD row's referenced key and
asks "is it still referenced by a child?"; for a benign non-key edit the key is
unchanged and the child legitimately still references it, so the answer is always
"yes" and the update is wrongly blocked. It is the exact mirror of the gap item 53
closed on the child side and item 117 closed for UNIQUE — the parent-UPDATE side
was simply never gated (and never tested in the false-positive direction).

## Fix

Add `has_fk_children_ref_in_set` = `has_fk_children` AND (a column that some child
references appears in `SET`), via a new helper
`fk_referenced_parent_columns(catalog, parent_def)`. Gate the three parent-side
sites on it: the outer constraint block, `acquire_fk_key_locks_parent`, and
`enforce_fk_restrict`. Result:

- Edit a non-referenced column (`shipping_address`, `order_number`, …) → RESTRICT
  skipped → **succeeds**.
- Change the referenced key (`id`) so a child would be orphaned → RESTRICT still
  fires → **correctly blocked**.
- Parent `DELETE` → unchanged (that path never used this gate) → still blocked when
  children exist.

`hot_eligible` left on `!has_fk_children` (conservative); making a parent update
HOT-eligible when the referenced key is unchanged is a follow-up perf item, not
needed for correctness (the per-row path is correct).

## Tests

- `tests/constraints.rs::fk_restrict_allows_parent_update_of_non_referenced_column`
  — asserts all three directions (A benign edit allowed, B key-change blocked, C
  delete-with-children blocked). Shaped on the real Studio scenario.
- Full `constraints` suite 30/30; crash harness 54/54; clippy + fmt clean.

## Testing-gap lesson (recorded in LESSONS.md)

The pre-119 FK tests covered parent RESTRICT only on **DELETE**, and only the
"it blocks" direction. Every constraint gate needs both directions — "still
enforces when it should" **and** "does not over-enforce when it shouldn't". The
false-positive direction for parent UPDATE had no test, which is exactly why this
shipped unnoticed.
