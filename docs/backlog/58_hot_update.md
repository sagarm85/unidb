# HOT-equivalent UPDATE (item 58)

**Type:** Performance
**Status:** IN PROGRESS

> D4 sign-off recorded in PROGRESS.md 2026-07-17. Implementation branch:
> `58-hot-update`. Honest ceiling: 0.07–0.09× PG (from 0.04×). See
> `57_next_perf_improvements.md` §A for the full architectural analysis
> and the conditions under which D4 sign-off was granted.

---

## Problem

UPDATE bulk is at 0.04× PG and has been confirmed as an **architectural
ceiling** without HOT (item 56 Step 2). The B-tree per-row insert dominates
(~35% of total UPDATE time at 50k rows). There is no way to skip it in
insert-new-version MVCC when an indexed column is in SET, because the B-tree
is the only forward resolver. Postgres serves `UPDATE t SET body=... WHERE
k<N/2` via **HOT** (Heap Only Tuple), achieving ~0.9M rec/s vs unidb's 35k
rec/s (23× gap) precisely because HOT avoids index maintenance entirely.

## Design

### Tuple header change (D4 sign-off)

The `_pad u16` at tuple-header offset [22..24] is repurposed as `hot_next: u16`
(sentined `0xFFFF` = no forwarding). TUPLE_HEADER_SIZE (24 bytes) is unchanged.
FORMAT_VERSION bumped 7 → 8.

### HOT update protocol

When `UPDATE t SET body = 'x' WHERE k < N` is executed and `body` is not
indexed and the old page has free space:

1. Acquire exclusive latch on the old page.
2. Check conflict (`xmax == 0`).
3. Check free space for new version on the same page.
4. If space: insert new version at new_slot on the same page.
5. Stamp `xmax = xid` on old_slot; set `hot_next = new_slot`.
6. WAL-log as single `WAL_HOT_UPDATE` record (one mini-txn).
7. B-tree is **NOT updated** — it still points to old_slot.

Readers following a B-tree candidate check: if `hot_next != HOT_NEXT_NONE`,
follow `hot_next` to find the current version on the same page.

### HOT eligibility gate

```rust
let hot_eligible = !has_unique
    && !has_fk_refs_in_set
    && !has_fk_children
    && !set_touches_indexed_col(assignments, &table_def.columns);
```

`set_touches_indexed_col` returns `true` if any SET clause target column
has either `index_root` or `unique_index_root` set.

If `try_hot_insert` returns `None` (page full), fall back to the standard
cross-page insert + B-tree update path.

### Vacuum interaction

When vacuum reclaims a HOT chain head (old_slot, xmax-stamped, below horizon):
instead of removing the B-tree entry `(key, old_slot)`, vacuum patches it
in-place to `(key, new_slot)` using `DiskBTree::update_rowid_inplace`.
Both `vacuum_inner` and `vacuum_table_inner` apply this logic. Failure to
do this was a bug discovered during testing (crash test P26 exposed it).

### WAL record: `WAL_HOT_UPDATE` (type 16)

```
redo: xid (8B) || old_slot (2B) || new_slot (2B) || insert_redo (variable)
undo: old_slot (2B) || new_slot (2B)
```

Redo: LSN-gated. Applies xmax + hot_next on old_slot; inserts new version at
new_slot. Undo: delete new_slot, clear hot_next on old_slot, clear xmax on
old_slot. Two-phase ordering (new-slot first, then old-slot) is crash-safe.

### Undo in the txn abort path

`UndoAction::HotUpdate { page_id, old_slot, new_slot }` calls
`Heap::undo_hot_update` which executes the two-phase undo atomically under
one WAL mini-txn.

## Files changed

| File | Change |
|------|--------|
| `src/format.rs` | FORMAT_VERSION 7→8, `WAL_HOT_UPDATE`, `HOT_NEXT_NONE` |
| `src/page.rs` | `hot_next` field in TupleHeader, `TH_HOT_NEXT` offset, `set_hot_next()` |
| `src/wal.rs` | `log_hot_update()` |
| `src/heap.rs` | `try_hot_insert()`, `undo_hot_update()`, HOT chain follow in `get_visible` |
| `src/recovery.rs` | `WAL_HOT_UPDATE` redo + undo arms, decode helpers, user-txn undo |
| `src/txn.rs` | `UndoAction::HotUpdate` variant, handling in abort loop |
| `src/sql/executor.rs` | `set_touches_indexed_col()`, `hot_eligible` gate, HOT path in `exec_update` |
| `src/lib.rs` | Vacuum HOT chain head handling in `vacuum_inner` + `vacuum_table_inner` |
| `tests/crash/main.rs` | P59a (WAL durable, page not flushed), P59b (incomplete user txn) |

## Crash tests

- **P59a**: HOT update committed (WAL durable), no checkpoint. Recovery re-applies
  WAL_HOT_UPDATE. B-tree query follows HOT chain → returns updated value.
- **P59b**: HOT update WAL-durable, user txn incomplete. Recovery undo restores
  old slot to live, makes new slot invisible. Original value returned.

## Acceptance criteria (from D4 sign-off and §57 analysis)

- UPDATE bulk ≥0.07× PG at 100k rows
- A7 regression guards: SELECT COUNT(*) ≥5×, SELECT filtered ≥0.50×,
  INSERT ≥0.50×, W4/W0 ≤2.3×
- Crash harness: 46/46 (2 new HOT tests: P59a/P59b)
- Concurrency matrix 32/32 PASS
- FORMAT_VERSION bump recorded in PROGRESS.md with D4 sign-off note

## Honest caveat

The 23× PG advantage on `UPDATE t SET body=... WHERE k<N/2` is the
**maximally favorable case** for HOT: unindexed column, dense-enough pages
with free space. Production workloads updating indexed columns, or tables
near full, see a smaller advantage. Honest ceiling: ~0.07–0.09× PG.
