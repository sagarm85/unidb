**Type:** Performance
**Status:** ⏳ NOT STARTED

# Item 69 — Fill factor / page reservation for HOT

## Problem

Items 58 and 71 (same-page and cross-page HOT) both reduce UPDATE cost by avoiding
B-tree patching. Same-page HOT (item 58) is the cheaper path — it keeps the new
version on the same page, so the B-tree chain head and the live version are on one
page, minimising I/O and recovery complexity.

However, same-page HOT fires only when the target page has slack space. With the
default FSM fill policy (pack pages until ~95% full), most UPDATEs on mature tables
fall through to the cross-page path (item 71), which still avoids B-tree patching
but introduces inter-page chains and more complex vacuum work.

PostgreSQL's fill factor (`FILLFACTOR`) reserves a fraction of each page exclusively
for UPDATE in-place upgrades — INSERT stops at e.g. 70% capacity, leaving 30% for
HOT rewrites. For UPDATE-heavy tables this is a major win.

## Design notes

- Expose `CREATE TABLE t (...) WITH (fill_factor = 70)` syntax (integer, 10–100,
  default 100 for backward compat).
- Store `fill_factor` in the per-table catalog entry (one byte, already has spare).
- In `heap.rs` FSM allocation: when choosing a page for INSERT, skip pages above
  `fill_factor %` full even if they have bytes free.
- The FSM `free_bytes` value already tracks per-page slack — the gate is
  `free_bytes < page_size * (1.0 - fill_factor/100)`.
- No format version bump needed: `fill_factor` is a catalog field, not a storage
  format field; old engines read the table with `fill_factor=100` (dense packing).
- B-tree indexes are unaffected (they manage their own fill factor internally).

## Acceptance criteria

- `CREATE TABLE t (id INT, body TEXT) WITH (fill_factor = 70)` accepted and stored.
- `INSERT` stops filling a page at the configured threshold.
- UPDATE throughput on a fill_factor=70 table improves vs fill_factor=100 at the same
  row count (same-page HOT fires more frequently → fewer cross-page chains).
- Docker bench comparison: UPDATE rec/s at 10k rows with fill_factor=70 vs 100.

## Dependencies

- Complements items 58 (same-page HOT) and 71 (cross-page HOT).
- No dependency on item 68 (hint bits) or item 70 (seq-scan prefetch).
