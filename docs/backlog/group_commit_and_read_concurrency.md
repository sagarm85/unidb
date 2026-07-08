# Group commit + read-only fsync skip (concurrency & durability performance track)

> **Note on the "M9" label.** The filename `m9_python_embedded_bindings.md`
> already claims the "M9" number for a *parked, not-started* effort. This
> track is a different, independent line of work (durability/concurrency),
> and its prototype is *implemented*, not parked. To avoid a number
> collision it is documented here descriptively rather than as "M9". The
> working branch is nonetheless named `m9-group-commit`. Milestone
> numbering for post-M8 work is the user's to sequence.

## Status as of 2026-07-08: PROTOTYPE LANDED (branch `m9-group-commit`)

Two of the three changes from the concurrency diagnosis are **implemented,
tested, and benchmarked**; one hardening item and one larger follow-on
remain (see [Remaining work](#remaining-work)).

- ✅ Read-only fsync skip (embedded + server paths)
- ✅ Group commit (server writer thread)
- ☐ Buffer-pool force-WAL-on-evict (required to make deferred-sync mode
  unconditionally safe for working sets larger than the buffer pool)
- ☐ Concurrent read path (readers off the single writer thread — the one
  genuine architectural change, an *addition* to existing MVCC)

---

## 1. Diagnosis (why this work exists)

Measured on M5 Pro (see `docs/performance/fssdb/`), every durable operation
sat on a ~3–4 ms floor while the underlying index primitives were ns–µs.
Two independent, compounding causes:

1. **Per-statement fsync.** One autocommit statement did **two** fsyncs:
   the per-statement *mini-txn* commit (`wal.rs::commit_mini_txn`, D2) and
   the *user-txn* commit (`wal.rs::commit_user_txn`, M1). `fsync` is
   ~1–3 ms; everything else is µs. This is the entire floor.
2. **Single writer thread.** The server (`server/engine_handle.rs`) funnels
   *every* request — reads included — through one OS thread that processes
   them strictly serially. So concurrent `POST /sql` throughput was
   **flat** (~131 → ~149 → ~153 ops/s at 1 / 10 / 50 clients): 50× the load,
   zero extra throughput. And because each commit fsync'd on its own, the
   fsyncs couldn't even overlap.

Both waste work the engine already did: MVCC (`mvcc.rs`) means readers need
no coordination, and ARIES no-force (D1) means individual statements never
needed their own fsync — only durability *at user-commit*, plus the
WAL-before-page invariant (D5), is required.

## 2. What the prototype changes

### 2a. Read-only fsync skip (`txn.rs`)
`TransactionManager::commit` skips `wal.commit_user_txn` (record **and**
fsync) entirely when `txn.undo_log.is_empty()`. A read-only transaction has
nothing to make durable. Safe because recovery classifies the orphan
`WAL_TXN_BEGIN` as an incomplete user txn whose undo pass finds no
mutations owned by that xid to reverse (verified in `recovery.rs`), and no
committed tuple's `xmin`/`xmax` ever references a read-only xid. This is
the fix for the M1.d "read-only commit pays an unnecessary fsync"
regression that had been flagged in `MEMORY.md` Open Questions since M1.

### 2b. WAL deferred-sync mode (`wal.rs`)
A new `deferred_sync: bool` field gates the fsync in **all four** durable
paths (`commit_mini_txn` / `abort_mini_txn` / `commit_user_txn` /
`abort_user_txn`); a new public `Wal::sync()` forces durability on demand.
**Off by default**, so the embedded API and the crash-injection harness
(D7) keep their per-statement durability guarantee unchanged — only a
single, serialized owner of the `Engine` turns it on.

### 2c. Engine passthrough (`lib.rs`)
`Engine::set_deferred_sync(bool)` and `Engine::sync_wal()`.

### 2d. Group commit in the writer thread (`server/engine_handle.rs`)
The writer thread turns deferred mode on, then **drains all queued requests
into a batch** (`blocking_recv` for the first, then `try_recv` until empty),
processes them, and issues **one fsync per batch** (`flush_pending`). A
`Commit`/`Abort` reply is **withheld** until that fsync completes, so a
client never observes a commit whose WAL record isn't yet durable. Reads
and not-yet-committed inserts reply immediately (their durability is only
promised at commit). If the batch fsync fails, every commit in the batch is
reported failed, since none are durable. `Checkpoint` forces a flush first
(it truncates the WAL).

Under concurrent load, a batch naturally contains many clients'
`begin`/`execute`/`commit` messages, so one fsync now covers many
transactions — the fsync cost amortizes across the whole batch. This is the
classic group-commit optimization and is exactly what closes the flat
throughput ceiling.

## 3. Benchmark results (M5 Pro, this machine, 2026-07-08)

### 3a. Concurrent HTTP INSERT throughput — the parallel scenario
`cargo bench --bench server --features server -- concurrent_http_throughput`
(N clients each `POST /sql` an INSERT into the *same* table).

| Clients | Before (per-iter) | Before ops/s | After (per-iter) | After ops/s | Speedup |
|---|---|---|---|---|---|
| 1  | 7.61 ms  | ~131 | 4.14 ms  | ~242   | 1.8× |
| 10 | 67.3 ms  | ~149 | 13.22 ms | ~756   | 5.1× |
| 50 | 326 ms   | ~153 | 10.46 ms | **~4,780** | **31×** |

The headline is the **shape**: throughput went from **flat** (the
single-writer ceiling) to **scaling with load** — more concurrency means
bigger batches means more fsync amortization.

### 3b. Read-only fsync skip — embedded point-SELECT
`cargo bench --bench load -- select_point`

| | Before | After |
|---|---|---|
| `select_point/point_get` | ~3.05 ms (M1 baseline) | **1.09 µs** |

~2,800× (criterion's saved-baseline diff: −99.965%). The fsync is entirely
off the read path.

## 4. Correctness

- **Locked decisions preserved.** D1 (steal+no-force) and D5 (WAL-before-
  page) are *upheld*: deferring the commit fsync only delays when
  `durable_lsn` advances; no page is flushed ahead of the durable WAL. D2's
  mini-txn remains the atomic unit; only *when* its WAL is forced changes,
  and only in deferred mode. No §3 decision is re-opened.
- **Durability contract.** Deferred mode is used **only** by the single
  writer thread, which owns the sole `Engine` handle. A commit is
  acknowledged to the client strictly after its batch fsync. A crash
  between an in-memory commit and its fsync leaves the txn without a durable
  `WAL_TXN_COMMIT`, so recovery undoes it — and the client, whose reply was
  withheld, correctly treats the outcome as unknown.
- **Tests.** 228 unit + 25 server integration + 11 crash-harness tests
  green; clippy `-D warnings` and fmt clean. The crash harness exercises
  the default (non-deferred) path, which is unchanged.

## 5. The known caveat (drives the hardening item)

In deferred mode `durable_lsn` lags within a batch. If a batch's working
set ever exceeds the buffer pool, the D5 eviction path
(`bufferpool.rs::find_victim`) — which *skips* dirty pages whose LSN is
ahead of `durable_lsn` — could fail to find a victim, surfacing as a failed
insert (`DbError::BufferPoolFull`-class), **not corruption**. It never
triggers in the tests or benchmarks (small working sets), but production
robustness requires the fix in [6a](#6a-buffer-pool-force-wal-on-evict).

## 6. Remaining work

### 6a. Buffer-pool force-WAL-on-evict (hardening — do before relying on deferred mode at scale)
Make the buffer pool force the WAL (a `Wal::sync` or bounded flush) when
eviction cannot find a victim because every candidate is ahead of
`durable_lsn`, instead of skipping/erroring. This is the proper ARIES
no-force behavior and is currently missing (it was never needed while every
mini-txn fsync'd). Requires the eviction path to reach a WAL handle (today
it does not) — a contained change. With it, deferred mode is
unconditionally safe. A cheaper interim mitigation is a batch-size cap in
the writer loop (`flush_pending` every N requests) to bound how many
non-durable pages can accumulate between syncs.

### 6b. Concurrent read path (the one real architectural change)
Take reads off the single writer thread. With MVCC snapshots already built,
`get`/`scan`/`NEAR`/`edges_from` can run concurrently on a read-only shared
view of the buffer pool, each holding only a snapshot; keep the
single-writer model for mutations (they are fsync-bound anyway, so
page-level write latching buys little once group commit exists). This
requires `BufferPool` to support concurrent shared page reads (per-frame
`RwLock` or a concurrent page cache) and splitting `Engine`'s API into
`&self` read methods vs. a serialized write lane. Result: **many concurrent
readers + one group-committing writer** — a standard, correct MVCC design
that fits the single-primary scope (CLAUDE.md §1).

## 7. Definition of done (for promoting this to a shipped milestone)
- 6a implemented; a deferred-mode durability test added alongside the crash
  harness (crash before batch sync ⇒ txn not durable; after ⇒ durable).
- 6b implemented and benchmarked (concurrent read throughput scaling).
- Benchmark tables recorded in `PROGRESS.md`; `README.md`/`docs/` updated.
- No locked decision (§3) violated.
