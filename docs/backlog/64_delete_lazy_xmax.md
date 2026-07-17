# DELETE selected performance investigation: lazy xmax feasibility + bottleneck profiling

**Type:** Performance
**Status:** INVESTIGATION COMPLETE — implementation pending (see Recommended approach below)

> Investigation produced 2026-07-17 on branch `64-delete-lazy-xmax` (worktree
> `unidb-item64-delete-hints`) under the §0.6 expert lens.
>
> Motivation: DELETE selected is measured at **0.04× Postgres** at 100k rows (bench
> Table 3, `030325` ARM Docker baseline). WAL was already confirmed not the
> bottleneck (WAL_XMAX_BATCH from item 56 Step 3 reduced WAL B/row from 230 to 74).
> The open question was whether the remaining gap is in xmax page-stamping, and
> whether "lazy" xmax (defer the in-page stamp until first read) could eliminate it.

---

## Profiling methodology

Added `UNIDB_DELETE_TIMING=1` instrumentation to `Heap::delete_many` (gated env
var; zero overhead when unset). Phases timed individually with `Instant::now()`:

| Phase | What it measures |
|-------|-----------------|
| `lock_acquire` | Row-level lock acquisition via `self.lock_manager.acquire_many` |
| `latch_fetch` | `pool.latch_exclusive(pid)` + `pool.fetch_page_for_write(pid, &wal)` per page |
| `fpi_check` | `pool.maybe_log_fpi(pid, &wal)` per page |
| `wal_batch` | `wal.log_xmax_batch(...)` per page (WAL append, no fsync with deferred_sync) |
| `xmax_stamps` | N × `page.set_xmax(slot, xid)` for all slots on one page |
| `set_lsn` | `page.set_lsn(lsn)` (one call per page) |
| `write_unpin` | `pool.write_page(&page)` + `pool.unpin(pid)` |
| `wal_commit` | `wal.commit_mini_txn(deferred_sync=true)` |

Test via `cargo test -- delete_many_timing --nocapture` with `UNIDB_DELETE_TIMING=1`
and `TIMING_ROWS=<total_rows>` (deletes the second half).

---

## Profiling results

### Run 1 — 25 000 rows deleted (small scale, warm pages)

| Phase | Time | % of total | Per-unit cost |
|-------|------|-----------|---------------|
| `xmax_stamps` | 20.21 ms | **87.5 %** | 809 ns/row |
| `lock_acquire` | 1.91 ms | 8.3 % | — |
| `latch_fetch` | ~0.3 ms | ~1.3 % | ~1.2 µs/page |
| `wal_batch` + `wal_commit` | ~0.4 ms | ~1.7 % | — |
| `write_unpin` + others | ~0.3 ms | ~1.3 % | — |
| **Total** | **23.1 ms** | 100 % | **1.08 M rows/s** |

Pages touched: ~241. Rows per page average: ~104 (25k / 241).
At this scale the buffer pool working set fits comfortably in L3 cache.

### Run 2 — 100 000 rows deleted (bench scale, TIMING_ROWS=200000)

| Phase | Time | % of total | Per-unit cost |
|-------|------|-----------|---------------|
| `latch_fetch` | 588.53 ms | **85.9 %** | **611 µs/page** |
| `xmax_stamps` | 80.69 ms | 11.8 % | 807 ns/row |
| `lock_acquire` | 10.67 ms | 1.6 % | — |
| `wal_batch` + `wal_commit` | ~2.5 ms | 0.4 % | — |
| `write_unpin` + others | ~3.0 ms | 0.4 % | — |
| **Total** | **685.4 ms** | 100 % | **145 k rows/s** |

Pages touched: ~963. Per-page latch+fetch cost: **611 µs vs 1.2 µs at small
scale — a 525× per-page slowdown** from Run 1 to Run 2.

### Key observation

Two entirely separate bottlenecks at different scales:

1. **CRC-per-mutation** (`xmax_stamps`): flat 807–809 ns/row regardless of scale.
   Dominates at small scale. Source: `set_xmax` calls `write_crc()` after each
   slot stamp — N calls per page group where N is rows on that page.

2. **latch+fetch scale blowup** (`latch_fetch`): 1.2 µs/page at 25k rows,
   611 µs/page at 100k rows. Dominates at bench scale. Source: mmap/OS-level
   cold-page cost when the working set is large (see §Root cause analysis below).

---

## MVCC analysis — lazy xmax feasibility

### Q1: How does a reader test visibility against a deleted row?

`is_visible()` in `src/mvcc.rs` reads `tuple_xmax` **directly from the tuple
header on the heap page** (bytes [8..16] of the 24-byte `TupleHeader`). When
`tuple_xmax == 0` the row is live; when `snapshot.is_committed_at_snapshot(xmax)`
is true, the row is logically deleted and invisible. There is no side-table, no
clog, no WAL consult — visibility is entirely a function of the in-page field.

### Q2: Can the xmax page-stamp be deferred (lazy)?

**No. Doing so is an MVCC correctness violation.**

If the xmax field on the heap page is not stamped at DELETE time, any concurrent
or subsequent reader would see `tuple_xmax == 0` and treat the row as live —
even after the deleting transaction commits. The row would be returned by every
`SELECT` until some asynchronous process eventually stamped the page, with no
bound on when that would happen. This breaks the fundamental MVCC invariant that
a committed DELETE is invisible to all transactions that start after the commit
(Read Committed isolation) or that took their snapshot after the commit
(Snapshot Isolation).

### Q3: Could a side-table (clog) replace the in-page stamp?

A clog alternative would work as follows: DELETE stamps a side-table entry
`(xid, committed)` and does NOT touch the heap page; every read of a non-zero
`xmax` then checks the clog. This is architecturally valid (it is how PostgreSQL
tracks commit status for hint bits) but imposes a **clog lookup on every row
visibility check** — including for live rows that have never been deleted.
In unidb's current access pattern (MVCC check per row per scan page), that is
one extra lock acquisition + hash-map lookup per row on the critical read path.
The DELETE write gain would be entirely offset by the read regression. Not viable
without a dedicated investigation of the read-path impact first.

### Q4: Does Postgres avoid the in-page xmax stamp?

No. Postgres writes `t_xmax` into the heap tuple at DELETE time, synchronously,
before the command returns. What Postgres defers lazily are **hint bits**
(`HEAP_XMAX_COMMITTED`, `HEAP_XMAX_INVALID`) — status flags that cache the
outcome of commit-log lookups so subsequent readers skip the clog. Hint bits
speed up *reads* after a DELETE has already committed; they do not eliminate the
initial xmax write. The Postgres DELETE write path is just as synchronous as
unidb's for the xmax stamp itself.

**Verdict: lazy xmax is infeasible. The in-page stamp at DELETE time is correct,
necessary, and architecturally mandatory under unidb's MVCC model.**

### Q5: Why is unidb behind Postgres on DELETE selected despite WAL_XMAX_BATCH?

Four factors, ranked by measured contribution at bench scale:

1. **CRC-per-mutation** (807 ns/row, 11.8 % at 100k, 87.5 % at 25k). Postgres
   has no page-level CRC that it recomputes on every field write; checksums in
   Postgres are optionally computed at whole-page flush time, not per-field.
   unidb's `set_xmax` calls `write_crc()` after every slot stamp — that is one
   8 KB heap allocation + 8 KB `crc32fast::hash` per row, N times per page group.

2. **mmap fetch+write overhead at scale** (611 µs/page at 100k). unidb copies
   pages in and out of the mmap on every write cycle (`fetch_page` reads 8 KB
   under a shared mmap `RwLock`; `write_page` copies 8 KB back under an exclusive
   mmap `RwLock`). At 963 pages this is 963 × 2 × 8 KB = 15 MB of memcpy under
   `RwLock` acquire/release cycles. The 525× per-page cost increase from 25k to
   100k suggests a cold-working-set effect: at 100k rows the delete operates on
   pages that were written during the insert phase and may have been partially
   evicted from OS page cache / TLB by the time deletion starts, causing mmap
   page-fault overhead per page.

3. **Higher page density in Postgres** — Postgres's tuple format is more compact
   and its `FILLFACTOR` logic can pack more rows per page, reducing total pages
   touched for a given selectivity.

4. **Mini-txn framing overhead** — one WAL `begin_mini_txn` + `commit_mini_txn`
   per page group is 2 WAL mutex acquisitions per page on top of the actual log
   append. At 963 pages this is 1926 WAL mutex acquires. Already measured as < 1 %
   of total at bench scale after WAL_XMAX_BATCH; not the limiting factor.

---

## Root cause: `set_xmax` CRC detail

```rust
// src/page.rs: set_xmax (line 438)
pub fn set_xmax(&mut self, slot: u16, xmax: Xid) -> Result<()> {
    // ... bounds check ...
    let base = offset as usize;
    self.data[base + TH_XMAX..base + TH_XMAX + 8].copy_from_slice(&u64_to_le(xmax));
    self.write_crc();  // ← 8 KB clone + crc32fast::hash EVERY CALL
    Ok(())
}

fn compute_crc(&self) -> u32 {
    let mut buf = self.data.clone();           // 8 KB heap alloc + memcpy
    buf[CRC_FIELD_OFFSET..CRC_FIELD_OFFSET + 4].fill(0);
    crc32fast::hash(&buf)                      // 8 KB hash
}
```

With `delete_many` calling `set_xmax` N times per page group then `set_lsn`
(which also calls `write_crc()`) once:
- **Total CRC computations per page**: N + 1
- **Total 8 KB heap allocations per page**: N + 1
- **Useful CRC computations per page**: 1 (the one in `set_lsn`, which sets the
  final page LSN and is the last write before the page goes to `write_page`)

The N intermediate CRCs from `set_xmax` are each immediately overwritten by the
next `set_xmax` call's CRC (or by `set_lsn`'s CRC). They serve no correctness
purpose: the page's CRC field is only checked on read (`verify_crc`), which
happens after the whole page group has been processed and `write_page` has stored
the final state. Intermediate CRCs are wasted work.

---

## Recommended approach

### Fix A — Defer `write_crc()` out of `set_xmax` (HIGH ROI, LOW RISK)

**Change**: Remove the `write_crc()` call from `set_xmax`. Let `set_lsn()` be
the sole caller that finalises the CRC after all mutations are done.

```
// src/page.rs: set_xmax — REMOVE write_crc() call
pub fn set_xmax(&mut self, slot: u16, xmax: Xid) -> Result<()> {
    // ... bounds check ...
    self.data[base + TH_XMAX..base + TH_XMAX + 8].copy_from_slice(&u64_to_le(xmax));
    // write_crc() call REMOVED — set_lsn() handles the final CRC
    Ok(())
}
```

`set_lsn()` already calls `write_crc()` and is always called after all
`set_xmax` calls in `delete_many` (and in every other heap write path that
touches multiple slots before calling `set_lsn`). The invariant is: the CRC
must be valid before the page is returned by `write_page` to the mmap — and
`set_lsn` is called before `write_page` in all write paths.

**Correctness**: The page's CRC is only checked at `verify_crc()` call time,
which happens (a) on `SlottedPage::from_bytes` (on every `fetch_page`) and
(b) in the crash-recovery redo path. Between `set_xmax` calls within a single
page group the page is exclusively latched and not readable by anyone — the
intermediate invalid-CRC state is invisible. The CRC written by `set_lsn` is
the one that gets persisted to the mmap by `write_page`, which is the only one
that matters.

**Scope of the change**: `src/page.rs` only. No WAL format change. No
FORMAT_VERSION bump. No recovery redo/undo change. Existing `set_xmax` call
sites outside `delete_many` (if any) that call it without a subsequent
`set_lsn` would need `write_crc()` added after them — audit required.

**Expected gain**:
- Small scale (25k rows, 241 pages, ~104 rows/page): `xmax_stamps` from 87.5 %
  to ~0.8 % of total (100× reduction in CRC work). Total DELETE time from 23 ms
  to ~2.7 ms → ~8× speedup at small scale.
- Bench scale (100k rows, 963 pages, ~104 rows/page): `xmax_stamps` from 11.8 %
  to ~0.1 %; but `latch_fetch` (85.9 %) is unchanged, so total speedup ~13 %.
  This will not close the 0.04× gap alone.

**Confidence**: High. The saving is deterministic: N-1 8-KB heap allocs + N-1
8-KB CRC hashes eliminated per page group. N ≈ 104 at current row density.

**Caveat**: There are other callers of `set_xmax` (HOT chain processing, recovery
undo). Audit each call site to confirm `set_lsn` follows before the page is
returned to the pool. If any call site does NOT call `set_lsn` afterward, add a
standalone `write_crc()` call at that site.

### Fix B — Investigate `latch_fetch` scale regression (MEDIUM ROI, NEEDS PROFILING)

The 525× per-page `latch_fetch` increase from 25k to 100k rows (1.2 µs → 611 µs)
is the dominant bottleneck at bench scale and is not explained by concurrency
(delete_many is single-threaded). Candidates:

1. **Cold mmap pages / OS page faults**: The test inserts 200k rows sequentially,
   then deletes the second 100k. By deletion time, OS page cache pressure may have
   evicted some of those mmap pages; the first access after eviction triggers a
   minor (or major) page fault. At 963 pages × 8 KB = 7.7 MB, this is plausible
   on macOS (M5 Pro) where mmap eviction policy differs from Linux.
   → **Diagnostic**: use `vm_stat` / `perf stat -e faults` to count page faults
     during the timing test. If faults ≈ pages touched, this is the cause.

2. **mmap RwLock acquire+release per page**: `write_page` acquires `mmap.write()`
   (exclusive `RwLock<PageFileMmap>`) for every 8 KB memcpy. At 963 pages,
   that is 963 exclusive lock acquires in tight succession. On macOS, `pthread_rwlock`
   uses futex-equivalent syscalls on contention; even uncontended exclusive locks
   have higher overhead than uncontended shared locks.
   → **Diagnostic**: add per-phase split between `pool.latch_exclusive` vs
     `pool.fetch_page_for_write` (the timing currently combines both).

3. **Buffer pool frame index HashMap growth**: `PoolState.frame_index: HashMap<PageId, usize>`
   grows as pages are loaded. At 963 pages, the HashMap is ~15× larger than at
   241 pages. HashMap lookup + insert is O(1) amortized but with higher constant
   at larger sizes (more L1/L2 cache misses in the bucket table).
   → **Mitigation**: pre-size the `frame_index` HashMap at pool construction.

**Recommended Fix B path** (for a follow-up item after Fix A is shipped):
- Split the `latch+fetch` timing into `latch_exclusive` vs `fetch_page_for_write`
- Add page-fault counting via `procinfo` or shell sampling
- If OS page faults: switch `fetch_page` from copy-on-read (current) to a
  direct mmap pointer return (no copy) — but this requires redesigning the
  exclusive-latch contract since the current design assumes an owned page copy
  is mutated then written back.
- If mmap RwLock: consider page-level CAS or per-page lock stripes instead of
  a single file-wide RwLock.

This is a larger architectural change. File as a separate item once the
diagnostic confirms the root cause.

---

## Not recommended: lazy xmax / clog

As established in the MVCC analysis above: lazy xmax breaks MVCC correctness.
A clog alternative imposes per-row clog lookups on every read. Neither path
improves DELETE without regressing reads. Do not pursue.

---

## Effort estimate

| Fix | Files changed | Effort | Risk |
|-----|--------------|--------|------|
| Fix A (CRC deferral) | `src/page.rs` only | 1–2h including call-site audit + regression run | Low |
| Fix B (latch diagnostic) | `src/bufferpool.rs`, timing test | 2–4h for diagnostic; follow-up item for the actual fix | Medium |

Fix A is a self-contained correctness-safe optimization. Ship it first and
measure against the `030325` baseline before filing Fix B.

---

## Acceptance target

- Fix A: `delete_many_timing` at 25k rows shows `xmax_stamps` < 5 % of total
  (from 87.5 %). At bench scale (100k rows, Docker `scripts/report.sh`), DELETE
  selected improves from 0.04× by ≥ 10 % (≥ 0.044×). A larger gain is possible
  but uncertain while `latch_fetch` dominates.
- All 48 crash tests pass. `cargo test` clean. `cargo clippy -D warnings` clean.
