# Autovacuum — background/triggered MVCC garbage collection

## Status as of 2026-07-09: **SHIPPED** (branch `autovacuum`, checkpoints A1–A4 as
## ordered commits). See `PROGRESS.md`'s "Autovacuum" entry.

**Implemented as the background-worker shape directly** (the spec's AV2), per the
Core-lane directive to add a `std::thread` launcher rather than an inline
`maybe_auto_vacuum()` from `commit` (AV1). This keeps *all* vacuum work off the
foreground commit path — the real autovacuum value — and is safe without new
locking because `Engine` is `Send + Sync` (Phase 5) and `vacuum` takes `&self`
under `write_serial` + per-page latches (M10). Honest divergences from the
proposal below, all documented in the shipped code + `PROGRESS.md`:

- ~~**Global** dead/live-tuple estimates (the spec's "global atomic in v1"
  option), not per-table. Whole-engine `Engine::vacuum` pass, not per-table
  `vacuum_table` — per-table accounting + `vacuum_table` + a cost-based
  throttle remain the documented AV-follow-up.~~ **RESOLVED (item 27,
  2026-07-13):** per-table estimates (`Engine::per_table_dead_estimate`,
  `per_table_live_estimate`, `tables_needing_vacuum`) + per-table
  `Engine::vacuum_table` + cost-based throttle (`VacuumCostConfig`) all shipped.
  The autovacuum worker loop now fires `vacuum_table` for each triggered table
  rather than a whole-engine pass.
- ~~**No bounded-K-per-call throttle.**~~ **RESOLVED (item 27, 2026-07-13):**
  `VacuumCostConfig` (page_hit_cost + page_dirty_cost + cost_limit + delay)
  bounds each pass via a `VacuumThrottle` that naps when the budget is spent.
  Default-on with Postgres-like values; configurable via
  `Engine::set_vacuum_cost_config`. Measured overhead: ~10× under tight budget
  (cost_limit=50, delay=2ms); negligible at default (cost_limit=200).
- **Whole-table compaction (V4) remains deferred.** See the deferral note in
  `27_vacuum_per_table.md` §V4 — requires a new multi-page WAL record type,
  which is a FORMAT_VERSION concern. Per-page compaction (M10.d `compact_page`)
  ships and handles intra-page bloat; V4 is cross-page defragmentation only.
- The worker holds a `Weak<Engine>` (a strong `Arc` would form a refcount cycle
  preventing `Engine::Drop`); the handle is an engine field, so field-drop is the
  clean-shutdown hook (M2.b-style, bounded join). Default-on for the served
  instance + the `Engine::open_arc` convenience; a bare `Engine::open` handle has
  no thread by construction (deterministic for tests; manual `vacuum()` stays).

---

## Original spec (as filed) — NOT STARTED (backlog). **Core-lane, engine work.**

Filed from the Postgres baseline comparison (`pg_baseline_comparison.md`, PR #25).
The churn test was the one place unidb clearly trails Postgres: under 30× update
churn, point-read latency degrades **6.8 → 35 µs** (MVCC version-chain bloat),
while Postgres's autovacuum keeps it flat. A **single manual `Engine::vacuum()`
restores unidb to 5.85 µs** (faster than fresh) — so this is an **automation gap,
not a capability gap**. The reclamation engine (M10) is already built and correct;
what's missing is *deciding when to run it without a human*.

## What exists today (M10)

- `Engine::vacuum() -> VacuumReport` — manual, whole-engine. Computes a safe
  horizon (`min xmin` over all live writer txns **and** live concurrent readers,
  M10.a), marks reclaimable versions DEAD, scrubs their secondary-index entries
  (the M10.c aliasing gate), compacts pages, promotes DEAD→UNUSED. Crash-safe
  (crash point P10), `WAL_VACUUM`-logged.
- `maybe_auto_checkpoint()` (P1.e) — the **precedent to copy**: fired from
  `Engine::commit`, gated by a time / WAL-size threshold, runs **only at a
  quiescent point** (`txn_mgr.active_count() == 0` so truncation can't discard an
  in-flight undo), configurable via env + `AutoCheckpointConfig`,
  `checkpoints_triggered` counter for observability.
- `Engine::stats()` (P6.g) — the place to surface autovacuum activity.

## What's missing

1. **A dead-tuple signal to trigger on.** There is no `n_dead_tup` equivalent.
   Need a cheap per-table (or at least global) counter incremented on every
   UPDATE/DELETE (each makes one version reclaimable), read to decide when a
   vacuum is worth it.
2. **A trigger policy.** Postgres: vacuum when
   `n_dead > threshold + scale_factor * n_live` (defaults 50 + 0.2·n_live). Adopt
   the same shape.
3. **A place to run it that doesn't stall the foreground.** M10 vacuum can be a
   long pass; running it inline on a committing thread (like auto-checkpoint) is a
   visible pause if unbounded.

## Design

Two stages, smallest-first — the sync engine default gets the inline version, the
server (which already has a thread pool) gets the true background worker.

### AV1 — dead-version accounting + bounded inline auto-vacuum (v1, no new thread)

- Add a dead-version counter (atomic per table, or a global atomic in v1) bumped
  wherever a version becomes reclaimable (UPDATE creates a superseded version;
  DELETE stamps `xmax`). Cheap, already on the write path.
- `maybe_auto_vacuum()` called from `commit` (exactly like `maybe_auto_checkpoint`),
  at a quiescent point, when the counter crosses
  `av_threshold + av_scale_factor * n_live`. Config via env
  (`UNIDB_AUTOVACUUM`, `_AV_THRESHOLD`, `_AV_SCALE_FACTOR`), default-on with
  conservative thresholds (don't trip existing tests, mirror auto-checkpoint).
- **Bounded work per invocation** (the one real difference from auto-checkpoint):
  vacuum at most *K* pages/versions per call so it never becomes a long foreground
  stall — incremental, amortized across many commits. Reset the counter by the
  amount reclaimed. `autovacuums_triggered` + `versions_autoreclaimed` in
  `Engine::stats()`.
- Correctness is inherited from M10: the horizon already respects live readers +
  writers, so a triggered vacuum can never reclaim a version someone still needs.

### AV2 — background autovacuum worker (v2, opt-in / server default)

- A dedicated worker thread (or a task on the server's existing blocking pool)
  that wakes on an interval, ranks tables by dead-ratio, and vacuums the neediest
  one — the classic autovacuum daemon. `Engine` is already `Send + Sync` (Phase 5),
  and vacuum takes `&self`, so this is safe without new locking. Keeps *all* vacuum
  work off the foreground commit path (the real autovacuum value).
- Requires **per-table dead-tuple accounting** (from AV1) and per-table
  `Engine::vacuum_table(name)` (M10 vacuum is currently whole-engine — factor it).
- Config: interval, per-table cost limits (a cost-based throttle like PG's
  `autovacuum_vacuum_cost_limit` so it doesn't saturate I/O).

## Recommendation

Ship **AV1 first** — it's small (copy `maybe_auto_checkpoint`, add a counter,
bound the work) and closes the benchmarked gap for the embedded engine with zero
new threads, keeping the "engine stays sync by default" invariant intact. Promote
to **AV2** for the server, where a background thread is idiomatic and fully hides
vacuum from foreground latency. AV2 needs per-table granularity, so build the
per-table dead counter + `vacuum_table` in AV1 even if AV1 triggers a whole-engine
pass, so AV2 is purely additive.

## Checkpoints (proposed)

- **AV1.a** — per-table dead-version counter (write-path increment) +
  `EngineStats` exposure.
- **AV1.b** — `maybe_auto_vacuum()` from `commit`, threshold policy, bounded
  incremental work, config + counters. Test: churn workload keeps point-read
  latency flat without a manual `vacuum()` call (the benchmark that motivated this).
- **AV2** — background worker thread, per-table `vacuum_table`, cost-based throttle;
  server default. (Own PR.)

## Non-goals / notes

- Not freeze/anti-wraparound (Postgres autovacuum also prevents xid wraparound).
  unidb's xid is `u64` (the M5 xid-reuse fix), so wraparound is not a near-term
  concern — out of scope here.
- Long-lived RR transactions / readers hold the horizon back and will stall
  reclamation (already surfaced as `VacuumReport.horizon_blocked`); autovacuum
  should surface the same in stats, not silently spin.
- Interacts with the durable-FSM work (`durable_fsm_catalog_pagelist.md`): both
  touch free-space accounting — share one source of truth so autovacuum's
  reclaimed space and the FSM don't drift.
