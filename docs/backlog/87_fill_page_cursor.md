**Type:** Performance
**Status:** ⏳ NOT STARTED

# Item 87 — Statement-scoped fill-page cursor

## Problem

`hot_update_many` / `update_many` Phase B (and `Heap::insert`) call
`acquire_page_for_insert` **per row**. Each call does FSM `free_map` hashmap
work (lookups, candidate collection, SipHash) and, on `main` before item 78's
O(1) grow, an O(N) scan; even after item 78 the per-row consult remains.

Measured (native `sample` profile of the items 75–84 branch, after the item 86
CRC prototype was applied): `acquire_page_for_insert` internals =
**~42% of remaining `exec_update` samples** — the largest single cost left on
the bulk UPDATE path.

## Fix

Hold a **statement-scoped fill-page cursor**: acquire a fill page once, keep
inserting new versions into it until `free_space() < needed`, then acquire the
next page — one FSM interaction **per page** (~50–100 rows), not per row.
Postgres analog: the relation-extension / bulk-insert target-block cache
(`RelationGetTargetBlock`).

- Scope: `hot_update_many` Phase B, `update_many` Phase B,
  `insert_batch_in_txn` (check whether item 84's merge-split already covers
  the insert path — do not duplicate).
- FSM is updated once per page transition with the final free space (the
  existing `note_free_space` call, moved out of the per-row path).
- Concurrency: the cursor holds no latch between rows in a batch — the page
  latch discipline of item 74/85 (A→B→C order, one latch at a time) is
  unchanged; the cursor only remembers *which* page to latch next.

## Expected gain

- UPDATE HOT: ~0.75× (post item 86) → **~0.85× PG**.
- Smaller gains on non-HOT UPDATE Phase B and bulk INSERT.

## Acceptance criteria

- Docker Table 3: UPDATE HOT ≥ 0.80× PG; INSERT/DELETE rows non-regressed.
- Crash harness green (Phase-B kill points unchanged); conc matrix scenario 10
  (cross-row-churn, toggle=on) 20/20 clean — this path was item 85's hang site.
