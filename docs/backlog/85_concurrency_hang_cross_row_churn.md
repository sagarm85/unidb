# Item 85 — Production-default concurrency hang: cross-row UPDATE churn, no index

**Type:** Improvement
**Status:** SHIPPED (→ PROGRESS.md "Item 85")

## Problem statement

Concurrency matrix scenario 10 (`w_cross_row_churn`, `toggle=on`, `indexed=false`,
8 writers × 8 rows) hung 1/3 times at 120 s. `toggle=on` is the production default
(`UNIDB_CONCURRENT_SQL_WRITES=1`). Additionally, after churn, the row count was
incorrect (3 rows instead of 2), indicating a correctness violation, not just a
liveness issue.

Scenario 9 (same workload but WITH a B-tree index) passed 3/3. Understanding the
difference was the key to the root-cause.

## Root cause

The `hot_update_many` function (item 74) used a **Phase B → Phase A** ordering:

1. **Phase B** (new inserts): insert new row versions on fill pages, committing a
   separate WAL mini-txn per fill page. After these commits, new tuples are on disk
   with `xmin = xid, xmax = 0`.
2. **Phase A** (xmax stamps): group old rows by page, latch each old page, conflict-
   check (`th.xmax != 0`?), stamp `xmax = xid`, write HOT_NEXT_XPAGE pointer.

The correctness violation: if Phase A encountered a `WriteConflict` on an old slot
(another writer had already stamped it), `hot_update_many` returned `Err(WriteConflict)`.
`exec_update` propagated the error and called `engine.abort(xid)`. However, **undo
entries for Phase B's inserts are recorded in exec_update's Phase 3**, which only runs
after `hot_update_many` returns `Ok`. On the error path, Phase 3 never ran — so the
undo log had no `HotXpageUpdate` entries for Phase B's committed tuples.

The abort then removed `xid` from `active` and added it to `aborted`. Future MVCC
snapshots use `is_committed_at_snapshot(xid)` which returns `true` for any xid that is
`< snapshot.xmax` and NOT in `active_xids`. Since `xid` was removed from `active`, the
next snapshot would treat Phase B's orphaned tuples (with `xmin = xid`) as belonging to
a committed transaction → permanently visible ghost rows.

**Why scenario 9 (with index) did not exhibit this:**
- With a B-tree index on the column in the SET clause: `set_touches_indexed_col = true`
  → `hot_eligible = false` → routed to `update_many`, not `hot_update_many`.
- `update_many` does Phase A (xmax stamps) FIRST, THEN Phase B (new inserts). If Phase A
  WriteConflicts, Phase B never runs and no orphaned inserts exist.

The hang came from livelock: ghost rows inflated the visible row set. Writers looped
in `txn_retry` indefinitely because the table state was corrupted.

## Fix

Swapped the phase ordering in `hot_update_many` from **B→A** to **A→B→C**:

- **Phase A (xmax stamp, `WAL_XMAX_BATCH`)**: conflict-check + stamp xmax on old slots
  BEFORE inserting new versions. WriteConflict aborts the mini-txn before any Phase B
  insert — no orphans possible.
- **Phase B (`WAL_INSERT_BATCH`)**: insert new row versions on fill pages after Phase A
  succeeds.
- **Phase C (`WAL_HOT_XPAGE_BATCH`)**: write the HOT_NEXT_XPAGE forward pointer on old
  slots now that `new_rid` is known. Phase C's WAL redo also re-stamps `xmax = xid`
  (idempotent after Phase A). Its undo payload includes `saved_prev_page/saved_prev_slot`
  for crash recovery.

The HOT chain pointer is needed for B-tree HOT chain following (item 71). It is written
in a separate pass (Phase C) instead of Phase A because `new_rid` is not known until
Phase B inserts are complete.

**Crash safety of the new ordering (D1/D2/D5):**
- Crash in Phase A before commit: no page writes. ✓
- Crash between A and B: `WAL_XMAX_BATCH` undo in the incomplete-user-txn recovery
  pass clears xmax → 0 on old slots. ✓
- Crash between B and C: recovery self-stamps `WAL_INSERT_BATCH` new versions dead
  (Phase 2 of the incomplete-txn undo). ✓
- Crash in or after Phase C: `WAL_HOT_XPAGE_BATCH` undo restores chain + clears xmax;
  `WAL_INSERT_BATCH` undo self-stamps new versions. ✓

## Test

Added `item85_cross_row_churn_no_index_no_hang` in `tests/concurrent_writers.rs`:
2 writers × 2 rows, opposite lock order, 40 rounds each, 10 s deadline, 5 repeats.
Confirmed failing before fix (3-row invariant + 10 s timeout), passing after fix.
