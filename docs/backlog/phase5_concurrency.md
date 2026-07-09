# Phase 5 — Concurrency & performance (Core lane)

## Status as of 2026-07-09: COMPLETE — all checkpoints (P5.a–P5.f) shipped.

Part 1 (P5.a buffer-pool latching, P5.b concurrent WAL append, P5.c concurrent
transaction manager, P5.d real lock manager) merged to `main` via PR #14. Part 2
(**P5.e** multiple writers — `Engine` is `Send + Sync`, an `Arc<Engine>` worker
pool, heap page latches, and leader-election group commit so write throughput
scales with cores (3.68× at 8 writers); **P5.f** resource control — per-query
timeouts/cancellation/`work_mem`) shipped on branch `p5e-concurrent-writers`
(PR #15). Crash harness **19/19** throughout; the sync invariant holds. Full
detail in the checkpoint sections below and `PROGRESS.md`'s Phase 5 entry.
**Documented limitation:** only raw CRUD scales with cores; SQL/graph/LOB writes
serialize (catalog `RwLock` / `Engine::write_serial`) — finer-grained
(latch-coupled B-tree) index concurrency is future work.

The single-writer → concurrent-writers unlock. **The biggest and highest-risk
phase** — it reverses the M5 "single writer thread, `Engine` is `!Sync`"
simplification. Companion to [`roadmap.md`](roadmap.md) §4. Serial Core lane
(one worktree). **Do NOT start before Phases 1 + 3 are solid** — concurrent
writers on a fragile storage core, or on RAM-bound rebuilt indexes, is
pointless.

## Context

Today all writes serialize through one thread; group commit (M9) only batches
their fsyncs. That's a hard one-core write ceiling. Production OLTP databases run
many concurrent writers coordinated by MVCC + row locks + physical latches. This
phase builds that — the largest change to the engine, and where the nastiest
bugs live (latch ordering, deadlocks, eviction races). Move carefully, test
adversarially.

## Scope

- **IN:** thread-safe storage core (buffer-pool latches, concurrent WAL append,
  concurrent txn manager), a real lock manager (modes / waits / deadlock
  detection), multiple writers + connection model, query timeouts / cancellation
  / per-query memory limits.
- **OUT:** distributed / sharded writes (parked); intra-query parallelism
  (follow-up).

## Checkpoints

### P5.a — Buffer-pool latching
- Page latches (shared/exclusive), concurrent pin/unpin/evict, latch-coupling
  for safe structure traversal. Rewrite the single-owner pool as concurrent.
- Files: `bufferpool.rs`. Preserve the D5 (WAL-before-page) invariant under
  concurrency.

### P5.b — Concurrent WAL append
- Multiple appenders with correct LSN ordering: serialized LSN allocation +
  group-batched flush (build on the M9 group-commit `deferred_sync`).
- Files: `wal.rs`.

### P5.c — Concurrent transaction manager
- Thread-safe `begin`/`commit`/`abort`/`snapshot` + active-txn set; the M10
  vacuum horizon must stay correct under concurrency (it already reads live
  readers/writers — verify under real threading).
- Files: `txn.rs`, `mvcc.rs`.

### P5.d — Real lock manager
- Shared/exclusive row locks, lock **modes**, **wait queues** (real blocking,
  not the current abort-only), **deadlock detection** via a wait-for graph.
  Replace the `try_acquire_write`-only lock table.
- Files: `lockmgr.rs`. Keep SI's first-committer-wins as one policy on top.

### P5.e — Multiple writers + connection model
- `Engine` becomes safely shareable (`Sync`) across N worker threads; a
  connection/worker pool; admission control. **This is the payoff — writes
  parallelize across cores.**
- Files: `server/engine_handle.rs`, `lib.rs`.

**Step 1 — DONE** (branch `p5e-concurrent-writers`, commit `75eaaa1`): `Heap` is
now interior-mutable `&self`, so **every** storage component is `&self`. The
`Sync`-Engine foundation is complete.

**Steps 2–4 — DONE** (branch `p5e-concurrent-writers`, 2026-07-09):
- **Step 2 (`0478db7`) — `Engine` is `Send + Sync`.** The 6 mutated fields became
  interior-mutable (`control → Mutex<ControlData>` + a cached immutable
  `page_size`; `next_lob_id`/`next_event_seq`/`checkpoints_triggered` → atomics;
  `auto_checkpoint`/`last_checkpoint` → `Mutex`), all 27 `&mut self` methods
  flipped to `&self`, and every vestigial `&mut BufferPool/Wal/…` signature/
  reborrow became `&`. `checkpoint::run` takes `&Mutex<ControlData>` and locks
  only for the small control update (never across an fsync). Compile assertion
  upgraded `Send` → `Send + Sync`.
- **Step 3 (`f977fb3`) — concurrent writers.** `server/engine_handle.rs` rewritten:
  `EngineHandle` holds `Arc<Engine>` and runs each blocking call on a tokio
  blocking-pool thread (`spawn_blocking`); the channel/`worker_loop` machinery is
  gone; read fast-path unchanged. **Heap page latches** (`BufferPool::
  latch_exclusive`, built in P5.a, finally wired) now wrap every heap RMW so
  concurrent writers can't lose an update; insert/update use a re-checking
  `acquire_page_for_insert`; latches are taken one page at a time (no two-latch
  deadlock). A coarse `write_serial` `Mutex` serializes the non-CRUD paths that do
  a non-atomic read-catalog-then-mutate-shared-index sequence (edges, LOBs, event
  tables, DDL, vacuum) — raw CRUD + reads stay concurrent. `tests/
  concurrent_writers.rs` (insert stress / distinct-row updates / same-row
  contention, all under a deadline guard).
- **Step 4 (`29fe805`) — group commit that scales.** `txn::commit` returns the
  commit LSN; `Engine::commit` forces durability via new `Wal::sync_up_to`, whose
  leader (`group_fsync`) runs `sync_all` **with the append lock released** so
  concurrent committers coalesce behind one fsync. Headline
  (`benches/concurrent_writers.rs`, 8 cores): **1→325, 2→330, 4→647 (1.99x),
  8→1197 (3.68x) commits/sec** — write throughput scales with cores.

**Remaining:** P5.f (below). Docs closeout + mark PR #15 ready.

**Step 2+ — measured execution plan (surveyed 2026-07-09; DONE per above).**
The remaining work was large but mechanical; the exact surface is known:
- **`Engine` → `&self`/`Sync` (`lib.rs`).** Only **6 fields are mutated** and
  need interior mutability; everything else is already `&self`:
  - `control: ControlData` → `Mutex<ControlData>` — **~44 access sites** (the
    bulk of the work; it's read all over and written in `checkpoint`/`open`).
    Watch lock scope: never hold the `control` lock across an fsync.
  - `next_lob_id` (2 sites), `checkpoints_triggered` (2), `next_event_seq` (3)
    → `AtomicI64`/`AtomicU64`.
  - `auto_checkpoint` (3), `last_checkpoint` (3) → `Mutex<_>`.
  - Then flip the **27 `&mut self` methods** to `&self`; the compiler +
    `cargo clippy --fix` propagate the `&mut`→`&` reborrows through call sites
    and tests (same technique used in P5.c/P5.e-1).
- **Writer/connection pool (`server/engine_handle.rs`, ~674 lines).** Today one
  dedicated OS thread owns the `Engine` and serves a channel (M5.a bridge).
  Replace with a pool of N worker threads sharing `Arc<Engine>` (now possible
  once `Engine: Sync`), keeping the read fast-path. This is a real rewrite of
  that module, not a tweak.
- **Then:** concurrent-writer stress + linearizability tests (no lost updates /
  torn state / deadlock hangs) and the **headline benchmark: write throughput
  scales with cores** → `PROGRESS.md`.

### P5.f — Resource control — DONE (`6f8e8c4`)
- Query timeouts, cancellation, per-query memory limits (a `work_mem` budget the
  hash-join/sort spills respect). Shipped as `query_limits.rs`: a thread-local
  `QueryLimits { deadline, cancel: CancelToken, work_mem_rows }` installed for
  the call via an RAII guard (a query runs on one worker thread). Executor scan
  loops call `query_limits::check()` every 1024 rows → `DbError::QueryTimeout` /
  `QueryCancelled`; `sort_mem_rows`/`hash_join_mem_rows` consult `work_mem_rows`.
  Entry point `Engine::execute_sql_with_limits`; server maps both errors to 408
  (and already has an HTTP `TimeoutLayer`). Tests: `query_limits` unit +
  `tests/query_limits.rs` end-to-end.

## Locked decisions touched

| Decision | Effect |
|---|---|
| Single-writer design (M5, implicit) | **Reversed** — record explicit human sign-off in `PROGRESS.md` per §3 |
| D11 (on_read/on_write seam) · D12 (SI) | Completed — real waits + deadlock detection, not abort-only |
| D5 (WAL-before-page) | Preserved under concurrency (P5.a) — non-negotiable |

## Verification gates (Phase 5 done =)

- Concurrency stress + linearizability tests: **no lost updates, no torn state,
  no deadlock hangs** under many concurrent writers.
- Write throughput **scales with cores** (the headline number).
- The full crash-injection harness + property test stay green under the new
  concurrent model.
- `clippy -D warnings` + `fmt` clean; PR per checkpoint; `PROGRESS.md`/
  `MEMORY.md` updated with the concurrency design notes.

## Known limitations / deferred

- No intra-query parallelism (parallel scans/joins) in v1 — a follow-up.
- NUMA / lock-free micro-optimization is later tuning, not correctness.
- Distributed / sharded writes remain parked (would reverse the single-primary
  charter).
