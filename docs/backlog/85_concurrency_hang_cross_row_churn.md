# Concurrency HANG — cross-row-churn, no-index, 8 writers

**Type:** Improvement
**Status:** NOT STARTED

> Surfaced by the PR #150 concurrency matrix (commit `7a25a5e`, 2026-07-19).
> Classified as a correctness item, not a performance item — a 120s hang
> under production-default settings is a potential release blocker.

## Observed symptom

From `docker/out/report_20260718_232622.md`, concurrency matrix:

| # | scenario | toggle | index | shape | result |
|---|---|---|---|---|---|
| 9 | cross-row-churn — paired UPDATEs per txn, opposite lock order | **on** | btree(k) | 8w × 8rows | ✅ PASS 3/3 |
| 10 | cross-row-churn — paired UPDATEs per txn, opposite lock order | **on** | **none** | 8w × 8rows | ❌ HANG 1/3 — exceeded 120s deadline |

- Toggle `on` = `UNIDB_CONCURRENT_SQL_WRITES=1` — **the production default since item 11**.
- The identical scenario WITH a B-tree index (scenario 9) passes 3/3.
- Without index, 1 of 3 repeats exceeds the 120s deadline (deadlock or livelock).
- The toggle=off, no-index case (scenario 7) also fails with duplicate ids — a pre-existing item-16 residual; out of scope here.

**Intermittency note:** 1/3 repeats PASS, 1/3 HANG. This is a probabilistic
deadlock (lock acquisition order depends on scheduler timing), not a guaranteed
hang. `CONC_REPEATS=10` or `CONC_ROUNDS=5` would tighten the detection window.

## Why it matters

Toggle=on is the shipping default. A 120s hang under concurrent UPDATE without
an index is a production-visible stall — any application doing concurrent writes
to a table without a secondary index (common: tables with only a PK, no
additional indexed columns) can trigger this. The fact that scenario 9 (same
workload, same toggle, but WITH btree index) passes 3/3 indicates the B-tree
locking path serializes in a way the no-index path does not.

## Root cause candidates

The cross-row-churn scenario issues paired UPDATEs per transaction in **opposite
lock order** (tx A updates row 1 then row 2; tx B updates row 2 then row 1). This
is the classic deadlock shape. The question is why the btree index path avoids it.

### Candidate 1 — Lock ordering via B-tree page latch
When a B-tree index exists, `update_many` acquires a B-tree page latch before
the heap write. The latch acquisition order may incidentally serialize the lock
sequence, breaking the circular wait. Without the index, no such external
serialization exists.

### Candidate 2 — Different code path under no-index
With `UNIDB_CONCURRENT_SQL_WRITES=on` and no index, `exec_update` may fall
through to a different path (e.g. not routing through `hot_update_many` or
`update_many`) that does not respect global lock ordering. Specifically:
- `hot_eligible` gate: HOT UPDATE path does not touch the B-tree.
- `update_many`: only used when `!has_unique && !has_fk`.
- No-index tables may use a simpler per-row path that acquires `LockManager`
  locks in scan order, which is not globally sorted.

### Candidate 3 — LockManager deadlock detection missing
The `LockManager` may have no deadlock detection (timeout-only). A true
cycle (A waits for B, B waits for A) blocks both threads until the 120s
deadline. The btree path may avoid the cycle by accident; the no-index path
hits it.

### Candidate 4 — Livelock in retry loop
If conflicting transactions retry without backoff, two concurrent writers can
live-lock indefinitely without a strict cycle. Check if the concurrent-write
retry path has bounded retries and backoff.

## Investigation plan

### Step 1 — Reproduce with a targeted test
Write a minimal test: 2 writers, 2 rows, no index, opposite lock order. Use
a 10s timeout. Confirm it hangs reliably at `CONC_ROUNDS ≥ 3`.

### Step 2 — Instrument lock acquisition order
Add temporary `tracing::debug!` in `LockManager::lock()` logging `(txn_id,
record_id)` pairs. Run the failing scenario. Look for A→B / B→A cycles in the
log.

### Step 3 — Compare with btree path
Add the same logging for the btree scenario (scenario 9). Identify what
structural difference prevents the cycle.

### Step 4 — Fix: sorted lock acquisition or deadlock detection
Two options (pick the simpler one that matches the evidence):

**Option A — Sort lock acquisition by `record_id` globally.**
Before acquiring any row lock in `exec_update`, sort the target row IDs in
ascending order. Ensures all writers acquire locks in the same order →
eliminates deadlock by construction. Pattern: 2PL with sorted lock ordering
(standard deadlock-avoidance technique).

**Option B — Add a deadlock timeout + abort-and-retry with backoff.**
If a lock is not acquired within N ms, abort the transaction and retry with
exponential backoff (cap at 3 retries). Simpler to implement but introduces
latency under contention.

Prefer Option A if the root cause is confirmed as lock ordering (candidate 1/2).
Option B is the fallback for the livelock case (candidate 4).

## Acceptance criteria

- [ ] Minimal 2-writer / 2-row / no-index test reliably reproduces the hang
      and reliably PASSES after the fix (CONC_REPEATS=10, timeout=10s).
- [ ] Concurrency matrix scenario 10 (toggle=on, no-index, 8w×8rows) PASS 3/3
      at `CONC_REPEATS=3` (matching the existing matrix baseline).
- [ ] Scenario 9 (toggle=on, btree, 8w×8rows) continues to PASS — no regression.
- [ ] All other 30 concurrency matrix scenarios continue to PASS.
- [ ] No throughput regression on UPDATE HOT / non-HOT benchmarks (lock ordering
      change must not add measurable overhead on the non-contended path).
- [ ] Root cause documented inline in this file (dated correction note per §0.6).

## Severity

**Medium-High.** Intermittent (1/3 repeats) and only on no-index tables under
highly concurrent opposite-order UPDATE churn. Not a data-corruption risk —
the hang times out and the transaction is abandoned. But it is a production stall
on the default-on code path, so it must be fixed before declaring concurrent
writes production-grade.

## Files likely touched

- `src/sql/executor.rs` — `exec_update`, lock acquisition order
- `src/lock_manager.rs` (or equivalent) — deadlock detection / timeout
- `tests/` — new minimal regression test
- `scripts/multi_model_report.sh` — concurrency matrix re-run to confirm fix

## Related

- Item 16 (`16_concurrent_sql_writes_visibility_anomaly.md`) — SHIPPED. The
  item-16 MVCC visibility anomaly was root-caused and fixed 2026-07-12. The
  current HANG is a separate issue (deadlock/livelock under no-index concurrent
  UPDATE, not a visibility anomaly).
- PR #150 (`perf/delete-update-v2`) — where the HANG was first observed in the
  concurrency matrix. Merged to main 2026-07-19.
