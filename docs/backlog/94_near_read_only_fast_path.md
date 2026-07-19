**Type:** Performance
**Status:** ⏳ NOT STARTED — after item 93

# Item 94 — Read-only NEAR fast path: skip MVCC snapshot for standalone queries

## Problem

Every `SELECT NEAR` today goes through the full `engine.begin()` →
`snapshot_alloc()` → beam search → `engine.commit()` cycle. Profiled overhead:

| Cost component | Estimate |
|---|---|
| `begin()` → snapshot allocation, WAL-tail pin | ~20–30 µs |
| `commit()` → active-xid unregister, lock table clear | ~10–20 µs |
| **Total txn overhead per NEAR query** | **~30–50 µs** |

pgvector runs the same full SQL/MVCC path at ~380 µs — so this is not
the *dominant* gap (item 93's arena layout is). But after item 93 brings
warm NEAR to ~580–620 µs, removing 30–50 µs closes most of the remaining
gap to pgvector-class (≤ 500 µs stretch target).

## Decision context

This item requires a design choice: **what is "safe" to skip?**

The NEAR query is a read. The only correctness requirement is that it sees
a consistent snapshot of committed rows. MVCC snapshot allocation in unidb:
1. Grabs the committed-xid epoch (O(1) atomic read).
2. Pins the WAL tail so vacuum can't advance past us.
3. Registers the snapshot in the active-snapshot list (for `txn_state()`
   lookaside).

For a **standalone read-only NEAR** (not inside a user BEGIN/COMMIT block):
- Step 1 is cheap and must stay (correctness: need committed epoch).
- Step 2 can be skipped if NEAR holds no long-lived page pins across vacuum
  cycles (beam search pin lifetime < 1 ms at warm path).
- Step 3 can be skipped if beam search never asks `txn_state()` for a
  partially-visible xmin — i.e. if we read only committed tuples (which is
  true when the L0/vec caches are pre-warmed and no in-flight insert is
  visible in our epoch).

**Safe condition:** NEAR is issued outside a user transaction (auto-commit
mode) on a table with no in-flight concurrent writers in the same epoch.
Concurrent readers are always safe (they don't change the committed set).

## What to build

1. **`read_snapshot_lightweight()`** in `transaction.rs`:
   - Reads `committed_xid_epoch` atomically.
   - Does **not** register in `active_snapshots`.
   - Does **not** pin WAL tail.
   - Returns a `ReadSnapshot { epoch }` (not a `Transaction`).

2. **Gate in `exec_select_near`** (executor.rs):
   - If called outside a user BEGIN block AND no explicit transaction is
     active on the connection → take `read_snapshot_lightweight()`.
   - Otherwise → fall through to full `engine.begin()` as today.

3. **Verify correctness**: a warm beam-search with the lightweight snapshot
   must return the same result set as the full-snapshot path for the same
   query at the same committed epoch. Property test: run the same NEAR query
   under both paths on a quiescent table; assert result identity.

## What NOT to do

- Do NOT skip the committed-epoch read — correctness boundary.
- Do NOT apply the fast path inside a user transaction (`BEGIN` ... `SELECT
  NEAR` ... `COMMIT`) — the user expects their preceding writes to be visible.
- Do NOT skip WAL tail pin when the query may hold page latches across a
  vacuum cycle boundary (conservative: always pin if `ef_search` > 500 or
  table has autovacuum in flight). Gate by `VACUUM_SAFE_EF_SEARCH_THRESHOLD`.

## Targets

- Additional warm NEAR latency reduction after item 93: **−30–50 µs**.
- Combined with item 93: ≤ **550 µs** warm at 10k → pgvector-class ✅.
- No correctness regression: result set identical to full-snapshot path on
  quiescent table; concurrent-writer stress test (32 writers, 1 NEAR reader)
  must show no phantom rows or stale-read violations.

## Acceptance criteria

- Step-0: measure txn overhead before building (instrument `begin()`/`commit()`
  wall time isolated); confirm ≥ 30 µs saving on warm path.
- Gate condition correctly enforced (unit test: fast path NOT taken inside
  explicit BEGIN block).
- Docker bench at 10k: ≤ 550 µs warm NEAR, recall@10 ≥ 0.90 held.
- Concurrency stress: 32 writers + 1 NEAR reader, 10k iterations, 0 panics
  / 0 incorrect results.

## ROI rationale

- ~30–50 µs gain is smaller than item 93 (−300–400 µs) but architecturally
  important: it removes the last structural overhead distinguishing unidb
  from a single-model vector library on pure read latency.
- Comes after item 93 (arena layout removes the bigger chunk first).
- No on-disk format change; no WAL format change; no FORMAT_VERSION bump.
