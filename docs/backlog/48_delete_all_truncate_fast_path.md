# DELETE all / TRUNCATE fast path: O(1) full-table wipe instead of per-row xmax loop

**Type:** Performance
**Status:** NOT STARTED
**Priority:** Medium — `DELETE FROM t` (no predicate) at 0.23× PG (+331%) is entirely avoidable. The operation's semantics allow a single WAL record + heap reset, but exec_delete currently loops per row.

---

## Measured gap (2026-07-15, MM_CRUD_ROWS=20000, matched fsync)

| operation | records | unidb (rec/s) | PG (rec/s) | ratio | WAL B/row | dec/row |
|---|---:|---:|---:|---|---:|---:|
| DELETE all | 20000 | 303 892 | 1 310 605 | 0.23× | 196 | 1.00 |

`dec/row = 1.00` — every row is fully decoded even though DELETE all has no predicate to evaluate and no WHERE filter to apply. `WAL B/row = 196` — one mini-txn WAL record per row (xmax stamp + page write). Postgres's `TRUNCATE` writes a single WAL record and frees/reuses pages in O(1); its `DELETE FROM t` also pays per-row xmax cost but is faster because its tuple header is in the page directly (no B-tree forward-resolver overhead).

## Root cause

`exec_delete` calls `matching_rows` with a null predicate (returns all rows), then loops calling `heap.delete(row_id)` once per matched row. `heap.delete` is a self-contained mini-transaction (begin + xmax + commit) per call. For N=20k: 20k mini-txn begins, 20k page latches, 20k WAL appends, 20k mini-txn commits.

There is no early-exit for the "delete everything" case. `exec_delete` doesn't distinguish `WHERE` clause vs no `WHERE` clause before dispatching.

Additionally, `matching_rows` decodes every row (`decode_row` → `dec/row = 1.00`) before the delete loop, even though DELETE only needs the RowId and the xmax stamp. The full decode is required for FK RESTRICT checks and CDC before-images — but for a no-FK, no-CDC table, this full decode is wasted.

## Fix: TRUNCATE SQL statement and heap-reset fast path

### SQL surface
Add `TRUNCATE TABLE t` to the parser. Semantics: delete all rows, reset the heap to empty (page 0 only), drop all existing index roots, rebuild empty indexes. In the same mini-transaction, write a single `TruncateHeap` WAL record (containing the heap's root page number and the old index roots for undo).

This matches Postgres's TRUNCATE semantics: atomically replaces the heap. FK RESTRICT must still check: if any other table has a FK referencing this table, reject the TRUNCATE unless CASCADE is specified. CDC: emit a single "truncate" event, not per-row DELETE events.

### Heap-level implementation
`Heap::truncate(&mut self, pool: &BufferPool, wal: &mut WalWriter) -> Result<()>`:
1. Log `WalRecord::TruncateHeap { heap_root, old_index_roots }` for undo.
2. Walk all pages; mark them as free in the FSM.
3. Reset heap metadata (root = empty, row count = 0).
4. For each index: reset to an empty B-tree root (or delete the root page and set index_root = 0 in the catalog).
5. Log `WalRecord::CommitMiniTxn`.

Undo path: restore the old root page and old index roots (the rows were never physically wiped — they're just unreachable; on undo, flip root pointers back).

### DELETE all → TRUNCATE opportunistic fast path (optional)
If the executor detects `DELETE FROM t` with no WHERE clause, no FK children, and no CDC subscribers, it can internally route to the heap-reset path without requiring the user to type TRUNCATE. This is an optimisation, not a correctness requirement.

### Crash safety
The WAL record must be written before the heap root is reset (D5). On redo: re-apply the root reset. On undo (crash before commit): restore old root pointers; the original pages are still intact.

## Acceptance criteria

- `TRUNCATE TABLE t` (or opportunistic DELETE-all routing): 20k-row table wipes in < 1 ms regardless of row count — O(num_pages), not O(rows).
- WAL B/row drops to ~0 when measured against the row count (one record total, divided by 20k rows → ~0).
- FK RESTRICT correctly blocks `TRUNCATE t` when a child table references `t`.
- CDC emits a single `{type: "truncate", table: "t"}` event (not 20k per-row DELETE events).
- Crash harness: (a) crash after WAL record, before heap reset → redo completes reset; (b) crash after heap reset, before commit → undo restores original rows.
- `PROGRESS.md` records before/after with absolute numbers.

## Depends on / builds on

- `src/heap.rs` — `Heap::truncate()`.
- `src/wal.rs` — new `WalRecord::TruncateHeap` variant.
- `src/sql/executor.rs` — `exec_delete` detection of no-WHERE path; `TRUNCATE` DDL dispatch.
- `src/sql/parser.rs` — `TRUNCATE TABLE t` parse rule.
- Item 36 (`36_foreign_key_row_enforcement.md`) — SHIPPED; FK RESTRICT check must cover TRUNCATE.
- Item 44 (`44_bulk_delete_batched_wal.md`) — batched WAL for partial DELETE; this item handles the full-delete case separately.
