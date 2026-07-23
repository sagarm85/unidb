# Parallel full-scan for DELETE selected (item 66)

**Type:** Performance
**Status:** ✅ SHIPPED 2026-07-18 — `parallel_collect_matching` in `parallel_scan.rs` with A3-gate-aware collect in `exec_delete`; 48/48 crash PASS; Docker bench pending. See `backlog_index.md` row 66 / PROGRESS.md "Item 66". _(Header corrected 2026-07-22 — was never flipped at ship time.)_

> Design from item 57 §B, calibrated on `report_20260717_151029.md` (100k rows,
> DELETE selected 238,747 rec/s = 0.04× PG; CRC fix item 64 not yet in that bench).

---

## Measured gap (post items 64+65, Docker Linux, expected from M2 closing bench)

| operation | records | unidb (rec/s) | PG (rec/s) | ratio | root cause |
|-----------|--------:|--------------:|-----------:|:-----:|:-----------|
| DELETE selected (k>=N, ~50% sel.) | 100000 | ~238k–390k est. | 5,365,898 | ~0.04–0.07× | Serial heap scan (~65% of delete_many time); delete_many WAL+page cost (~35%) |

After item 64 (CRC fix, WAL_XMAX_BATCH), the WAL bottleneck is gone (~74 B/row).
The dominant cost is now the **serial full-scan phase** in `exec_delete → matching_rows`.
At 100k rows (~1250 pages), the serial scan visits each page once, sequentially.
The pre-spawned 18-worker pool already parallelises COUNT(*), filtered SELECT, and
GROUP BY; wiring it into DELETE gives the same ~10–12× scan speedup seen on those paths.

## Design

Implemented in two places:

### 1. `src/sql/parallel_scan.rs` — `parallel_collect_matching`

New function added after `parallel_count_matching`. Pattern:
- Each worker maintains a **thread-local** `Vec<(RowId, Vec<u8>)>` (no mutex in hot loop).
- Work-steal pages via `AtomicUsize` cursor (same as all parallel paths).
- `scan_page_into` reads one page; each visible tuple is passed to `matches(&bytes)`.
- After pool completion, per-worker Vecs are concatenated into one output.
- **Caller must sort** by `(page_id, slot)` before passing to `delete_many` (item 44 B5).

### 2. `src/sql/executor.rs` — `exec_delete`

The existing single `matching_rows` call is replaced with a labeled block `'collect`:

1. Mirror the A3 gate: if `find_best_indexable_btree_predicate` + `index_lookup_is_selective`
   would select the index path, skip the parallel attempt (let `matching_rows` handle it —
   the index path is already fast).
2. Otherwise: `heap.scan_pages()` → `acquire(pages.len())` → if lease obtained,
   call `parallel_collect_matching` with the B2-masked predicate closure.
3. Sort the result by `(page_id, slot)`.
4. On any gate miss (A3 fires, table too small `< PARALLEL_CANDIDATE_MIN` pages,
   no lease): fall through to `matching_rows` as before.

The pre-check pass (FK RESTRICT, CDC event capture), `delete_many`, SSI undo logging,
and `persist_pages_if_changed` are **unchanged** — they operate on the sorted
`matching` Vec regardless of which path produced it.

## Correctness analysis

| Risk | Verdict |
|------|---------|
| MVCC visibility per worker | Safe: workers call `scan_page_into` with a cloned (read-only, Arc) `Snapshot` — identical to the serial path. |
| Lock acquisition ordering | Safe: `delete_many` calls `try_acquire_write_many` on the full sorted RowId set; `page_id` order prevents deadlock with other concurrent statements. |
| Write conflict detection | Safe: the parallel collect is read-only; conflict detection happens inside `delete_many` per-page. |
| CDC ordering | Safe: pre-check loop (CDC capture) runs serially after collect, over the sorted RowIds — same order as serial path. |
| FK RESTRICT | Safe: same as CDC — runs serially after collect. |
| A3 gate interaction | Safe: when A3's index path fires, the parallel path is bypassed entirely; `matching_rows` handles it unchanged. |
| SSI read-set tracking | Safe: `ssi_note_reads` is called after collection, same as before. |
| Small table | Safe: `PARALLEL_CANDIDATE_MIN` (64 pages) prevents parallel overhead from hurting small-table DELETE. |

## Expected improvement

At 100k rows (~1250 pages, 18 workers):
- Scan phase: ~65% of DELETE time → parallelises ~10–12× → net 5.5–7× speedup on scan portion
- delete_many: ~35% → unchanged
- Net: `0.65T/10 + 0.35T ≈ 0.415T` → **~2.4× total speedup**
- Expected: 238k rec/s → ~570k rec/s (with CRC fix already in, starting point is higher)
- vs PG 5.4M rec/s → **~0.10–0.15×** (from ~0.04–0.07×)

## Acceptance criteria

- DELETE selected ≥ 0.10× PG at 100k rows (from ~0.04–0.07×).
- All existing DELETE correctness tests pass.
- Crash harness green (no new crash points — scan is read-only; `delete_many` unchanged).
- Concurrency matrix 32/32 PASS.
- `PROGRESS.md` records before/after rec/s numbers.

## Files changed

| File | Change |
|------|--------|
| `src/sql/parallel_scan.rs` | Added `parallel_collect_matching` (~85 lines) after `parallel_count_matching` |
| `src/sql/executor.rs` | Replaced `matching_rows` call in `exec_delete` with `'collect` block (~55 lines) |
| `docs/backlog/66_parallel_delete_scan.md` | This file |
| `docs/backlog/backlog_index.md` | Item 66 registered |
