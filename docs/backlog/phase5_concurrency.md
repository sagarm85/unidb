# Phase 5 — Concurrency & performance (Core lane)

## Status as of 2026-07-08: NOT STARTED.

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

### P5.f — Resource control
- Query timeouts, cancellation, per-query memory limits (a `work_mem` budget the
  hash-join/sort spills respect).

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
