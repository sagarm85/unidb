**Type:** Performance
**Status:** ⏳ NOT STARTED (sequence LAST among items 86–90 — touches the item-85 subsystem)

# Item 88 — Bulk DML lock-table elision (xmax as the tuple lock) + batched undo

## Problem

Bulk DML takes a **per-row lock-table entry**: `try_acquire_write_many`
inserts one `RecordId` per row into the global lock table (two full passes
under one mutex), and `release_all` at commit iterates **every** lock entry
(`for e in t.locks.values_mut()`) — O(100k) under the global lock-manager
mutex for a 100k-row DELETE.

Measured (native `sample` profile, items 75–84 branch):
`try_acquire_write_many` / `LockTable::grant` is the **top CPU item inside
`delete_many`** (32 of 33 subtree samples), and visible in `hot_update_many`.

The entries are redundant for bulk statements: the `xmax != 0` conflict check
under the page latch — already performed by `delete_many` / `update_many` /
`hot_update_many` before any mutation — provides first-writer-wins on its own.
This is Postgres's design: no heavyweight lock per tuple; the xmax stamp *is*
the tuple lock. Blocking waits exist only for UniqueKey/FkKey phantom locks,
which stay untouched.

## Fix

1. Skip per-row lock-table entries in `delete_many`, `update_many`,
   `hot_update_many`. Conflict detection = the existing xmax check under
   latch (fail-fast `WriteConflict`, same NoWait semantics as today).
2. Keep UniqueKey / FkKey phantom locks (blocking, deadlock-detected) exactly
   as they are.
3. Single-row paths (`update`, `delete`, `try_hot_insert`) keep the lock table
   for now — they interoperate correctly because they *also* check xmax.
4. Batched undo (was R6): one `UndoAction::XmaxStampBatch { page_id, slots }`
   per page group instead of one `XmaxStamp` per row — matches the
   WAL_XMAX_BATCH granularity; abort applies the batch in reverse.

## Expected gain

- DELETE selected: 0.81× → **~0.90× PG** (the last measured serial cost).
- Commit latency: `release_all` drops from O(all rows touched) to O(phantom
  locks held).

## Risks / gating

- **This is the subsystem item 85's hang lived in.** Root cause there was
  phase ordering, not the lock table, but the gate is strict anyway:
  conc matrix full pass ×3 **plus scenario 10 at 20/20 clean repeats**, and a
  two-writer interleaving test proving single-row vs bulk conflict detection
  still agrees (bulk writer stamps xmax → single-row writer gets
  WriteConflict, and vice versa).
- SSI hooks (`ssi_note_write`) are orthogonal (they don't read the lock
  table) — assert unchanged behavior in the serializable test suite.

## Acceptance criteria

- Docker Table 3: DELETE selected ≥ 0.86× PG; UPDATE rows non-regressed.
- Conc gates above; crash harness green (undo batch replay case added).
