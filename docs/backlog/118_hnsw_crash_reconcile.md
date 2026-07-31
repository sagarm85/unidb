# Item 118 — Async-HNSW crash-durability hole: reconcile the lost queue tail on open

**Type:** Improvement
**Status:** ✅ SHIPPED 2026-07-31 (branch `feat/hnsw-crash-reconcile`) — background
crash-gated reconciliation + clean-shutdown drain + idempotency guard + a real
crash-convergence test. Full suite + crash harness green.

## The hole (from the 2026-07-30 architecture review)

On the served / `Engine::open_arc` path, a vector-row INSERT commits the heap row
durably, then **enqueues** the HNSW index insert onto the worker's in-memory
`sync_channel(4096)`. The worker WAL-logs each insert **when it runs**, so WAL
redo restores everything the worker durably applied — but the **un-drained channel
tail is lost to a crash** (those inserts were never WAL'd), and recovery had **no
HNSW reconciliation step**. Net: rows committed to the heap could be **permanently
missing from the index** and invisible to `NEAR`. Worse, there was **no `Drop` on
`Engine`**, so even a *graceful* exit dropped the un-drained tail. The only crash
test (`item67_async_hnsw::async_hnsw_crash_safety`) covered the sync path and
asserted heap survival only.

## What the investigation established

1. Only the async path is affected (bare `Engine::open` inserts inline + WAL'd
   before commit returns).
2. WAL redo restores everything the worker durably applied; the only lost set is
   the in-memory channel tail — there is **no durable watermark/queue-depth**, so
   the gap must be *derived*, not read.
3. `DiskHnswIndex` keeps a `node_index` DiskBTree (RowId → node loc), so per-row
   membership is an O(log n) lookup. But `insert` is **not idempotent** — re-adding
   a present RowId writes a duplicate node. So reconciliation must diff, not blindly
   re-insert.
4. O(1)-open moat: reconciliation must not scan the heap on *every* open.

## Design (shipped)

Extend the existing async freshness contract *across restarts*, reusing the worker
channel and a `node_index` diff — **not** a per-insert WAL marker (rejected: it
taxes the commit path the async design exists to keep clean).

- **`hnsw.dirty` sidecar marker** (next to `control`/`data.db`/`db.wal`; no
  control-file format bump). Created when the worker spawns; a leftover marker at
  open means a previous async session didn't cleanly drain. Gates reconciliation so
  clean reopens stay O(1). Chosen over a control-file flag because the control file
  is `FORMAT_VERSION`-gated.
- **`spawn_hnsw_worker`** (covers both server and `open_arc`): if the marker was
  present, spawn a one-shot **background** reconcile thread — open returns
  immediately.
- **`Engine::reconcile_hnsw_indexes`**: for each HNSW-indexed column, scan the
  committed heap; for each RowId not `contains()`-ed by the index, re-enqueue
  through the normal worker (so the repaired insert is WAL'd). Returns the count.
- **`DiskHnswIndex::contains(rid)`** (new `pub`): used by reconcile to enqueue only
  the missing tail, and by the worker before every insert to make inserts
  **idempotent** — this closes the race between reconcile and a concurrent live
  insert of the same row, and folds in the worker's previously-swallowed
  insert-error leak.
- **`Engine::flush_hnsw_for_shutdown`** (clean-shutdown path, wired into
  `EngineHandle::shutdown`): drains the worker, then clears the marker. Must run
  while the engine is alive — `Drop` is too late because the worker's
  `Weak<Engine>` can no longer upgrade once the strong count hits 0 (so `Drop`
  cannot drain the tail). If it isn't called (crash, or an exit that skips it), the
  marker persists → next open reconciles. **The reconcile-on-open is the
  correctness guarantee; the clean drain is an O(1)-open optimization.**

## Tests

- `tests/item118_hnsw_reconcile.rs`:
  - `hnsw_reconciles_committed_rows_after_unclean_shutdown` — insert 60 vector rows
    async, drop WITHOUT draining (simulated crash; diagnostics observed ~27 rows
    still queued and only ~34/60 NEAR-findable right after reopen), then assert the
    index **converges to 60/60** via reconcile. Genuine regression test — with the
    reconcile trigger neutered it stays at ~34/60 and times out.
  - `hnsw_clean_shutdown_clears_marker_so_reopen_skips_reconcile` — clean drain
    clears the marker and the reopen finds all rows.
- Full suite + crash harness (54/54) green; clippy + fmt clean.

## Follow-ups (not in scope)

- HNSW is still a single background worker (the Table-4 moat bottleneck at scale) —
  parallelism is a separate perf item.
- `hot_eligible` for parent updates and other queued review findings are unrelated.
