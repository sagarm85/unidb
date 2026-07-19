**Type:** Performance
**Status:** ⏳ NOT STARTED

# Item 98 — /sql INSERT throughput: 127 rows/sec → competitive via statement batching

## Problem

compare.py seeds unidb at **~127 rows/sec** via `/sql` (multi-row VALUES
INSERT, batch=75). The same data loads into Postgres at **~43,000 rows/sec**
via psycopg2. The gap is ~340×.

Observed times:
| table | rows | unidb | PG |
|---|---|---|---|
| customers | 1,000 | 8.3 s | 0.0 s |
| orders | 2,000 | 16.1 s | 0.0 s |
| order_items | 5,963 | 46.9 s | 0.1 s |

This is a real engine limitation, not a demo script issue. Any user who seeds
data via the REST `/sql` endpoint will hit this.

## Root cause hypothesis (profile before building)

Three candidates, ranked by likelihood:

### H1 — fsync per `/sql` call (most likely)

Each `/sql` POST is one mini-txn = one WAL commit = one `fsync()`.
On macOS with `F_FULLFSYNC`, `fsync()` inside Docker is
**50–200 ms** (Apple Hypervisor VM → virtualdisk → macOS flush).

```
14 batches × 100ms/fsync ≈ 1,400 ms  →  doesn't explain 8,300 ms alone
```

But combined with H2 and H3, compound effects explain the total.

Step 0 must measure actual fsync latency inside the running container.

### H2 — per-row mini-txn in multi-row VALUES INSERT (critical to verify)

If the executor's INSERT path calls `heap.begin_mini_txn()` /
`heap.commit_mini_txn()` **per row** inside a VALUES list (rather than once
per statement), a 75-row batch pays 75 fsyncs instead of 1:

```
1,000 rows × 1 fsync/row × 8ms/fsync = 8,000 ms  ✓ matches observed 8.3s
```

This would be a bug in the multi-row INSERT execution path — the whole
VALUES clause should be ONE mini-txn.

### H3 — FK enforcement per row at large batch sizes

`order_items` has two FK checks per row (order_id → orders, product_id →
products). At batch=75, each HTTP call executes 75 × 2 = 150 B-tree lookups.
These are fast individually but compound with H1/H2.

## Step 0 — measure before building

Before implementing anything:

1. **Time fsync inside Docker**: `docker exec <container> dd if=/dev/zero of=/tmp/fsync_test bs=4k count=1 oflag=sync` — measure time per call.
2. **Instrument mini-txn count per INSERT statement**: add a debug counter `MINI_TXN_COUNT` (feature-gated); assert it = 1 for a 75-row VALUES INSERT. If it = 75, H2 is confirmed.
3. **Profile a single 75-row INSERT via HTTP**: `tracing` spans for `execute_sql` → `exec_insert` → `heap.commit_mini_txn`. Report where wall time is spent.

## What to build (after Step 0 confirms root cause)

### Fix A — enforce one mini-txn per VALUES INSERT statement (if H2 confirmed)

The executor should wrap the entire VALUES list in a single mini-txn:

```rust
// exec_insert for multi-row VALUES
let mut mini_txn = heap.begin_mini_txn()?;
for row_values in values_list {
    heap.insert_versioned_in_txn(&mut mini_txn, row)?;
}
mini_txn.commit()?;   // one WAL commit + one fsync
```

This is the correct ACID behaviour (the whole INSERT statement is atomic)
and eliminates N−1 unnecessary fsyncs.

### Fix B — WAL group commit for sequential /sql calls on the same connection

Even with Fix A, each `/sql` call is one fsync. A client issuing 14 sequential
INSERT batches pays 14 fsyncs. **WAL group commit** batches concurrent or
pipelined commits into one fsync:

```
call 1 commits → appends WAL_COMMIT record, waits for sealer
call 2 commits → appends WAL_COMMIT record, waits for sealer
sealer wakes → fsyncs once, signals both → both return durable
```

Implementation: a `GroupCommitter` background task:
- Writers append their WAL_COMMIT record and add themselves to a `pending`
  queue + condition variable.
- GroupCommitter wakes periodically (configurable: `UNIDB_GROUP_COMMIT_DELAY_US`,
  default 500 µs) or when the pending queue hits N entries.
- One `fsync()` covers all pending commits; all writers are unblocked.

This is a well-understood technique (PG's group commit, MySQL's binlog group
commit). The key invariant: no writer returns "committed" before the fsync that
covers its WAL_COMMIT record completes.

**Group commit and item 89 (WAL background sealer) interact**: the background
sealer handles segment-seal fsyncs; group commit handles per-commit fsyncs.
They use separate sync paths (segment-seal vs commit-fsync) and do not
conflict.

### Fix C — pipelined /sql over a persistent connection (HTTP/1.1)

If the Python client is not reusing HTTP connections (new TCP handshake per
call), each of the 14 INSERT batches pays ~5ms Docker networking overhead:

```
14 × 5ms = 70ms  (minor vs fsync cost, but measurable)
```

The unidb axum server already sends `Connection: keep-alive`. The fix is
to verify this is working and document that clients should use a connection
pool (Python `requests.Session`, not bare `urllib.request.urlopen`).
This is a docs fix, not an engine fix.

## Targets

After Step 0 identifies the root cause and Fix A/B land:
- 1,000-row seed via `/sql` (batch=75): **≤ 2 s** (down from 8.3 s, ≥4×).
- 5,963-row `order_items` seed: **≤ 8 s** (down from 46.9 s, ≥5×).
- compare.py total seeding: **≤ 15 s** (down from 118.6 s, ≥8×).
- INSERT throughput via `/sql` multi-row VALUES: **≥ 5,000 rows/sec**.
- No regression on single-row INSERT correctness or crash tests.

## Acceptance criteria

- Step-0 report committed before any fix (diagnosis must be evidence-based).
- If H2 confirmed: regression test that a 1000-row VALUES INSERT is exactly
  1 mini-txn (counter asserted, crash harness: whole batch atomic).
- If group commit: concurrent-writers test (32 threads, each issuing /sql
  INSERT, measure total wall time and fsync count; assert fsync count <<
  total commits).
- compare.py seed section: unidb total seeding ≤ 15 s on Docker/Mac.
- All existing crash tests (P* set) pass unchanged.

## ROI

- compare.py seeding takes 118.6 s vs 0.3 s for PG — the FIRST thing any
  user sees. A 400× INSERT gap makes the engine look broken before the
  benchmark even runs.
- Fix A alone (one mini-txn per VALUES statement, if H2 is confirmed) may be
  a 50–75× improvement with minimal code change.
- Group commit (Fix B) benefits all write-heavy REST workloads, not just
  seeding — every INSERT/UPDATE/DELETE call through /sql benefits.
