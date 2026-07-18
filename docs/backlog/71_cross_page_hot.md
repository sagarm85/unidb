**Type:** Performance
**Status:** ✅ SHIPPED 2026-07-18 — cross-page HOT chains; FORMAT_VERSION 8→9; WAL_HOT_XPAGE_HEAD record type 17; P_xhot_a + P_xhot_b crash tests; 50/50 crash + 431 unit tests PASS. See PROGRESS.md "Item 71".

# Item 71 — Cross-page HOT chains (extend item 58)

## Problem

Item 58 (same-page HOT) eliminates the O(log n) B-tree update cost when the new
version fits on the same page as the old version. But for packed tables — where every
UPDATE fires on a full page — same-page HOT never fires and the engine falls back to
a regular insert-new-version + B-tree patch. At 100k rows the measured UPDATE
throughput was **0.07× PG** (14× behind Postgres).

The root cause: same-page HOT requires spare space. Most real workloads (high-write,
low-vacuum frequency) run with nearly-full pages.

## Solution: cross-page HOT chains

When a page is full and the update is HOT-eligible (no indexed column in SET):
1. Insert the new version on **any** page with space (normal `acquire_page_for_insert`).
2. Stamp `xmax` on the old slot and write a cross-page forwarding pointer into the old
   slot's repurposed `prev_page` / `prev_slot` fields (activated by sentinel
   `hot_next == HOT_NEXT_XPAGE = 0xFFFE`).
3. The B-tree is **not updated** — it still points at the old (chain-head) slot.
   Forward scans follow the chain to find the live version.

This eliminates the O(log n) B-tree patch on every UPDATE on a full page.

## Implementation (shipped)

### New constants (`format.rs`)
- `HOT_NEXT_XPAGE: u16 = 0xFFFE` — sentinel distinguishing cross-page from same-page HOT
- `WAL_HOT_XPAGE_HEAD: u8 = 17` — WAL record type for old-page changes (xmax + chain ptr)
- `FORMAT_VERSION: u32 = 9` — bumped from 8; older recovery would skip type 17 via `_ => {}`

### WAL record layout (`wal.rs`)
- `log_hot_xpage_head(txn_id, prev_lsn, old_page_id, xid, old_slot, new_page_id, new_slot,
  saved_prev_page, saved_prev_slot)`
- redo payload (16B): `xid(8) || old_slot(2) || new_page_id(4) || new_slot(2)`
- undo payload (8B): `old_slot(2) || saved_prev_page(4) || saved_prev_slot(2)`
- Atomicity: `WAL_INSERT` (new version) + `WAL_HOT_XPAGE_HEAD` (old page) in one mini-txn

### Page layer (`page.rs`)
- `set_hot_xpage(&mut self, slot, xpage_pid, xpage_slot)` — writes chain pointer
- `restore_prev_and_hot_next(&mut self, slot, saved_prev_page, saved_prev_slot)` — undo

### Heap layer (`heap.rs`)
- `HotInsertResult { new_rid, saved_prev: Option<(PageId, u16)> }` — cross-page variant
- `try_hot_insert` FSM pre-screen: `fsm_says_full=true` → skip same-page, go straight to
  cross-page HOT
- `get_visible` updated: checks `HOT_NEXT_XPAGE` before `HOT_NEXT_NONE`
- `get_visible_with_rid` / `Heap::get_resolved`: returns live RowId after chain follow
  (critical for `index_matching_rows` to avoid re-updating an already-xmax-stamped chain head)

### Latch ordering
New-before-old: acquire new page latch (insert), release, then acquire old page latch
(xmax + chain pointer). No deadlock: no other code path holds both simultaneously.

### Recovery (`recovery.rs`)
- redo: `WAL_HOT_XPAGE_HEAD` → apply `set_xmax` + `set_hot_xpage` on old page
- undo: `WAL_HOT_XPAGE_HEAD` → `restore_prev_and_hot_next` + clear `xmax`
- M1 user txn undo: Phase 1 = old page restore via undo payload; Phase 2 = new version
  death via `WAL_INSERT` xmin self-stamp path (existing)

### Vacuum (`lib.rs`)
Both vacuum passes updated to follow `HOT_NEXT_XPAGE` chains in addition to same-page chains.

## Crash tests
- `P_xhot_a`: cross-page HOT WAL durable, page not flushed → crash → updated value recovered
- `P_xhot_b`: cross-page HOT incomplete user txn → crash → original value restored, new version invisible

## Measured result
See PROGRESS.md "Item 71" for Docker bench numbers.

## Target
UPDATE throughput: **0.07× PG → 0.40–0.55× PG** (6–8× improvement)
