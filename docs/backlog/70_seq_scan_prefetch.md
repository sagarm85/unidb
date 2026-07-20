**Type:** Performance
**Status:** ✅ SHIPPED — see PROGRESS.md "Item 70" entry (2026-07-20)

# Item 70 — Sequential scan prefetch (read-ahead)

## Problem

The sequential scan path (`scan_page_visit` / `exec_select_fullscan`) reads pages
one at a time via `pool.fetch_or_create(page_id)`. Each call may trigger an mmap
fault / OS page-in if the page is cold. On a dense sequential scan this is a
cache-miss-per-page pattern when the OS prefetcher has not yet kicked in.

PostgreSQL uses `posix_fadvise(POSIX_FADV_SEQUENTIAL)` on heap files and issues
explicit read-ahead requests during seqscans. On Linux this triggers OS-level
prefetching; on macOS the equivalent is `F_RDADVISE` / `fcntl(F_RDAHEAD)`.

For unidb's mmap-based storage, the equivalent is:
1. Call `madvise(MADV_SEQUENTIAL)` on the heap file range at open time (already done?).
2. During a seq-scan, issue `madvise(MADV_WILLNEED, start, prefetch_window)` a few
   pages ahead of the current cursor to hint the kernel to prefetch those pages
   while the engine is processing the current one.

## Design notes

- Add `prefetch_ahead(page_id: PageId, n: usize)` to the buffer pool / mmap layer
  in the page module. The call is a best-effort hint — if the platform doesn't
  support `madvise` it is a no-op.
- Issue the hint from `scan_page_visit` whenever `page_id` advances (i.e., in the
  sequential scan loop) with a configurable look-ahead window (default: 8 pages = 64 KiB).
- `#![forbid(unsafe_code)]` exemption: `madvise` is already in the `unsafe` mmap
  module — document the safety invariant (pointer within mapped range, length aligned).
- Benchmark: measure cold-cache seqscan latency at 100k rows with and without the hint.
  The gain is most visible on the first full scan after DB open (cold mmap pages).

## Acceptance criteria

- Seq scan on a cold (just-opened) DB shows measurable latency improvement at ≥100k rows.
- No correctness regression (all 50 crash tests + unit tests green).
- No-op graceful fallback on platforms where `madvise` / `MADV_WILLNEED` is unavailable.

## Dependencies

- Independent of all other items. Can be developed in a parallel worktree.
- Complements item 54 (arena alloc, already shipped) and item 59 (late materialisation).
