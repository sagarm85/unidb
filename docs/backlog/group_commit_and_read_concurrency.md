# Group commit + read-only fsync skip (concurrency & durability performance track)

> **Note on the "M9" label.** The filename `m9_python_embedded_bindings.md`
> already claims the "M9" number for a *parked, not-started* effort. This
> track is a different, independent line of work (durability/concurrency),
> and its prototype is *implemented*, not parked. To avoid a number
> collision it is documented here descriptively rather than as "M9". The
> working branch is nonetheless named `m9-group-commit`. Milestone
> numbering for post-M8 work is the user's to sequence.

## Status as of 2026-07-08: PROTOTYPE + HARDENING LANDED (branch `m9-group-commit`)

Three of the four changes are **implemented, tested, and benchmarked**; one
larger follow-on remains (see [Remaining work](#remaining-work)).

- ✅ Read-only fsync skip (embedded + server paths)
- ✅ Group commit (server writer thread)
- ✅ Buffer-pool force-WAL-on-evict (deferred-sync mode is now
  unconditionally safe for working sets larger than the buffer pool; also
  fixes the pre-existing `BufferPoolFull`-at-scale limitation for the
  ordinary per-statement-fsync path)
- ✅ Concurrent read path (branches `m9-concurrent-reads` + `m9-concurrent-select`):
  point reads (`get` / `GET /rows/:id`) **and** read-only SQL `SELECT`
  (`POST /sql`) now run off the single writer thread on a `Send + Sync`
  `ReadHandle` over shared state (`Arc<RwLock>` mmap + catalog, `Arc<Mutex>`
  txn snapshot). `NEAR` and writes stay on the writer thread by design. See
  [6b](#6b-concurrent-read-path-the-one-real-architectural-change).

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

## 5. The former caveat — now fixed by 6a

In deferred mode `durable_lsn` lags within a batch, so if a batch's working
set exceeded the buffer pool the old D5 eviction path
(`bufferpool.rs::find_victim`) — which merely *skipped* dirty pages ahead of
`durable_lsn` — could dead-end at `DbError::BufferPoolFull` (a failed
insert, **not corruption**). **Resolved 2026-07-08 by [6a](#6a-buffer-pool-force-wal-on-evict).**

## 6. Remaining work

### 6a. Buffer-pool force-WAL-on-evict — ✅ DONE (2026-07-08)
**Implemented.** The buffer pool now tracks the durable WAL frontier
(`durable_wal_lsn`, a stale-low-safe lower bound refreshed on every
write-path fetch and on `Engine::sync_wal`). `find_victim` writes back and
evicts a dirty page once its LSN is durable (ARIES steal), instead of just
skipping it. The new `BufferPool::fetch_page_for_write(page_id, &mut Wal)` —
used by every heap write/undo path and the FSM scan — refreshes that
frontier and, if the pool is still full of *not-yet-durable* dirty pages,
forces one `Wal::sync()` and retries (ARIES "force the log before stealing
the page", D5). Reads keep using plain `fetch_page` (reads never dirty
pages). Proven by `bufferpool.rs::
fetch_for_write_forces_wal_sync_to_evict_nondurable_dirty_pages`; the crash
harness stays green, confirming the new write-back-on-evict path preserves
recovery correctness.

**Bonus:** this also fixes the pre-existing `BufferPoolFull`-at-scale
limitation (discovered M6) for the ordinary per-statement-fsync path — dirty
pages are now evictable once durable, where before the pool could only ever
evict clean frames.

### 6b. Concurrent read path — ✅ DONE for reads (point reads + SQL SELECT)
Take reads off the single writer thread. Chosen structure: **shared read
handle**, not full interior-mutability of the engine (which would have put a
`Mutex` on the buffer-pool frames — a `find_victim`-must-flush reentrancy
trap). The writer keeps owning `Engine` with `&mut self` writes unchanged;
only the read-relevant state is shared.

**Landed (Phase 1a foundation + point reads):**
- `bufferpool.rs`: `mmap` → `Arc<RwLock<PageFileMmap>>` (the guard against a
  reader seeing a torn or remapped-away page); a `PageReader` trait (the read
  seam) + `SharedPageReader` (frame-free reader view). Writer methods stay
  `&mut self`, locking the mmap internally.
- `heap.rs`: `get`/`scan` are generic over `PageReader` (reads copy the page
  out — no pin/unpin).
- `txn.rs`: `TransactionManager` state behind `Arc<Mutex<TxnInner>>`
  (`SharedTxn`); `read_snapshot()` builds a self-contained READ COMMITTED
  snapshot for a read that allocates **no xid and writes no WAL**.
- `read_handle.rs`: `ReadHandle` (`Send + Sync + Clone`) with `get(row_id)`;
  `Engine::read_handle()`. Server `GET /rows/:id` dispatches to it via
  `spawn_blocking`, bypassing the writer channel entirely.
- Proof: `tests/concurrent_reads.rs` (4 readers hammering committed rows
  while the writer inserts 1000 — every read returns exact bytes, no tears);
  `benches/server.rs::concurrent_read_throughput` shows reads *scale* with
  concurrency (~3.0k → ~4.3k → ~4.5k reads/s at 1/10/50, HTTP-client-bound in
  the microbench) instead of the flat single-writer ceiling.

**Concurrent SQL `SELECT` — done (branch `m9-concurrent-select`):**
- `Engine.catalog` → `Arc<RwLock<Catalog>>` (readers need the live
  `TableDef.pages`, which grows on INSERT). The writer takes the write-lock
  only per statement — never across an fsync (in group-commit mode the fsync
  is a later, separate step), so readers block only briefly.
- `executor::exec_select_readonly` — a `PageReader`-generic full-scan SELECT
  reusing `decode_row`/`predicate_matches`/`project_row`; `plan_is_concurrent_read`
  classifies a plan (plain `SELECT`, no `NEAR`).
- `ReadHandle::execute_sql` (read-only) + `is_concurrent_read_sql`; server
  `post_sql` routes concurrent-readable SQL to the read handle and everything
  else (writes, DDL, `NEAR`) to the writer thread.
- Lock order is consistent (catalog → txn → mmap) on both the writer and
  reader sides, so no inversion/deadlock. Proven by
  `tests/concurrent_reads.rs::concurrent_sql_select_...` (4 readers `SELECT`
  while the writer inserts 500 rows; every row's `name` pairs with its `id`).

**Not on the concurrent path (by design, still writer-thread):** `NEAR`
(needs the HNSW index fast path), `edges_from`/Cypher, and `poll_events` —
each is the same additive pattern (`edges_from`/`poll` would share
`EdgeIndex`/the event heaps) if a future workload needs them concurrent.

## 7. Definition of done (for promoting this to a shipped milestone)
- ✅ 6a implemented; force-WAL-on-evict unit test added; crash harness green.
- ☐ A deferred-mode durability test alongside the crash harness (crash
  before batch sync ⇒ txn not durable; after ⇒ durable) would further
  harden the group-commit path — not yet added.
- ✅ 6b: point reads **and** SQL `SELECT` implemented, benchmarked (`GET`
  read scaling), and covered by two concurrency correctness tests. `NEAR`/
  graph/queue reads stay on the writer thread by design (additive if needed).
- ✅ Benchmark tables recorded in `PROGRESS.md`; `README.md`/`docs/` updated.
- ✅ No locked decision (§3) violated (D1/D2/D5 upheld). `Engine` stays
  non-`Sync`; the new `ReadHandle` is the `Send + Sync` shared reader.
