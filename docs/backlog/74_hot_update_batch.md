# Batch mini-txn for HOT UPDATE

**Type:** Performance
**Status:** ✅ SHIPPED (commit 4dd81ac) — hot_update_many Phase B+A; covered by the items 75–84 Docker bench (UPDATE HOT 453k rec/s / 0.62× vs PG). See `backlog_index.md` row 74 / PROGRESS.md items 75–84. _(Header corrected 2026-07-22 — was never flipped at ship time.)_

## Problem

`exec_update` calls `try_hot_insert` (or `heap.update`) **per matched row**. Each call opens
its own `begin_mini_txn` → WAL record(s) → `commit_mini_txn` bracket. At 50k matched rows:

- 150k serialised passes through `Mutex<WalInner>` (3 per row: begin, record, commit)
- 150k `Vec::new()` heap allocations for WAL record buffers
- 150k CRC32 passes over ~177-byte records

Total: **~0.05× PG** at 100k rows (32,678 rec/s vs 623,769/s). The user transaction already
groups ALL 50k writes under ONE fsync — the bottleneck is pure mutex/alloc/CRC CPU overhead,
not I/O.

Confirmed by Fable 5 architect review (2026-07-18): the honest bottleneck is the WAL record
production cost per mini-txn, not I/O or column decode.

## Fix: two-phase exec_update + `Heap::hot_update_many()`

Mirrors the existing `update_many` Phase A/B split already proven by item 44/56.

### Phase 1 (per-row SQL logic — unchanged cost)

Collect all `(old_rid, encoded_bytes, before_row, after_row)` tuples by running the existing
per-row SQL logic: decode, eval SET, encode, basic constraint checks (NOT NULL, CHECK).
No heap writes yet.

### Phase 2 (batched heap writes)

`Heap::hot_update_many(rows: &[(RowId, Vec<u8>)], ...) -> Result<Vec<(RowId, RowId, PageId, u16)>>`

**Phase B first** (new versions, fill pages):
- Pack as many new row versions per fill page as fit
- One mini-txn per fill page: `WAL_BEGIN` + N×`WAL_INSERT` + `WAL_COMMIT`
- Records `(old_rid, new_rid)` pairs in input order

**Phase A second** (old versions, HOT chain — cross-page):
- Group by `old_rid.page_id`
- One mini-txn per page group:
  `WAL_BEGIN` + `FPI?` + N×`WAL_HOT_XPAGE_HEAD` + `WAL_COMMIT`
- Sets `xmax = xid` + `hot_next = HOT_NEXT_XPAGE (0xFFFE)` on each old slot
- Reads `saved_prev_page/slot` from tuple header for WAL undo payload

Phase B before Phase A preserves the "new-before-old" latch ordering invariant (item 71 / heap.rs
doc comment on latch ordering).

### Phase 3 (post-write, per pair)

`record_undo(UndoAction::HotXpageUpdate {..., saved_prev_page, saved_prev_slot})`
+ `ssi_note_write` + CDC event — identical to current per-row path.

## Reduction in mini-txn overhead

| | Before (per-row) | After (Phase A+B) |
|---|---|---|
| Mutex acquisitions | 150k (3/row × 50k) | ~2k (2 × ~1k page groups) |
| Vec allocations | 150k | ~2k |
| CRC32 passes | 150k | ~2k |

~75× reduction in WAL bookkeeping overhead.

## Correctness invariants

- **D2**: Each mini-txn is one-page-at-a-time. Phase B: one fill page per mini-txn. Phase A: one
  old page per mini-txn. Both are single-page — no cross-page atomicity required.
- **D5**: WAL-before-page enforced: WAL_INSERT / WAL_HOT_XPAGE_HEAD written before `pool.write_page`.
- **Crash between Phase B and Phase A**: Phase B mini-txns committed → new versions in pages.
  Old versions: xmax = 0, hot_next = HOT_NEXT_NONE (unchanged). User txn (xid) is uncommitted.
  MVCC: new versions (`xmin = xid, uncommitted`) invisible. Old versions visible. ✓ Correct.
- **Undo**: Phase B's WAL_INSERT undo (xid self-stamp → invisible) + Phase A's WAL_HOT_XPAGE_HEAD
  undo (clear xmax + restore hot_next + restore saved_prev). User-txn undo follows existing paths.
- **Lock ordering**: acquire all write locks in one pass (`try_acquire_write_many`) before Phase B.
- **Gate**: `hot_eligible` (same as current try_hot_insert guard: `!has_unique && !has_fk_refs_in_set
  && !has_fk_children && !set_touches_indexed_col`) — CORRECTNESS gate, not tuning knob.

## Honest ceiling (Fable 5 review)

- Remaining per-row costs: `decode_row`, `eval_expr`, `encode_row`, `lock_mgr` write, `record_undo`
- Conservative estimate: 0.20–0.40× PG (not the previously-stated 0.40–0.55×)
- Non-HOT UPDATE remains at ~0.07× structural ceiling (B-tree patch overhead dominates)
- DELETE selected at ~0.14× structural ceiling (page-write phase bottleneck — no HOT path)

## Acceptance criteria

- `cargo test` all green (≥430 tests + ≥50 crash harness)
- Local bench: UPDATE HOT throughput measurably higher than 32,678 rec/s baseline
- Docker bench: UPDATE HOT ratio > 0.10× PG at 100k rows (vs 0.05× current)
- No regression on SELECT / DELETE / INSERT
- Crash harness: add a P74 point (crash between Phase B and Phase A) + verify old versions survive
