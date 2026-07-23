**Type:** Performance
**Status:** ✅ SHIPPED 2026-07-20 (PR #170) — group-commit dwell window in WAL `sync_up_to`; `PUT /config/group_commit_window_us`; `Engine::wal_fsyncs_count()`. See `backlog_index.md` row 101 / PROGRESS.md. _(Header corrected 2026-07-22 — was never flipped at ship time.)_

# Item 101 — Group commit (WAL flush batching)

## Problem

Every single-row INSERT today costs one `fdatasync` per commit.
Item 89 (WAL background sealer, shipped) moved the fsync off the caller's
critical path — a background thread fsyncs and signals waiters. But each
commit still waits for *its own* fsync ack: one fsync → one commit woken.

Postgres avoids this via its `wal_writer` process: when N sessions all commit
within the same flush cycle, they share one `fdatasync`. The per-commit cost
drops from O(n × fsync_latency) to O(1 × fsync_latency ÷ n_concurrent).

At Mac M5 Pro / NVMe SSD, `F_FULLFSYNC` ≈ 1–2 ms. Under 8 concurrent INSERT
sessions each inserting 1,000 rows sequentially: without group commit, each of
the 8,000 inserts waits ~1 ms = ~8 s total. With group commit, sessions sharing
a flush pay 1 ms ÷ 8 ≈ 0.125 ms per commit → ~8× throughput improvement.

**This is the largest remaining lever for INSERT per-row on concurrent workloads.**
Single-session sequential INSERT is unaffected (still 1 commit per fsync —
the architecture floor). That is fine: single-session INSERT throughput is already
within ~50% of SQLite; the multi-session case is where unidb falls furthest behind.

## Architecture (builds on item 89's background sealer)

The WAL background sealer (`wal.rs`) currently:
1. Waits for a `Notify` signal from any committing writer.
2. Calls `fdatasync` on the WAL file.
3. Updates `durable_lsn`.
4. Wakes any thread waiting on `durable_lsn ≥ its commit_lsn`.

Group commit adds one step between (1) and (2): **collect stragglers**.
After waking, sleep a brief **group window** (e.g. 100 µs, configurable via
`UNIDB_GROUP_COMMIT_WINDOW_US`, default 0 = disabled) before fsyncing. During
the window, additional committing threads append their WAL records and queue into
the waiter list. After the window, one fsync flushes all of them together.

The waiter mechanism already exists (threads block on `durable_lsn ≥ commit_lsn`).
No new lock or queue is needed — the background sealer just delays its own fsync
slightly to amortize cost across concurrent waiters.

### Key invariant

D5 (WAL-before-page) is unaffected: dirty pages are still not evicted until
`page.LSN ≤ durable_WAL_LSN`. The background sealer sets `durable_lsn` after
the fsync as today; the only change is it may batch more records into one fsync.

### Configuration

```
UNIDB_GROUP_COMMIT_WINDOW_US=100   # 100 µs batch window; 0 = off (default)
```

Also exposable via `PUT /config/group_commit_window_us` (same pattern as
`PUT /config/slow_query_threshold_ms` from item 34).

## Expected gain

| Workload | Before | After (estimated) |
|---|---|---|
| 8 concurrent INSERT sessions, 1k rows each | ~53% of PG | ~80–90% of PG |
| Single-session sequential INSERT | ~53% of PG | unchanged |

The gain is strictly on concurrent workloads. The benchmark (`report.sh`) uses
concurrent INSERT via the conc-matrix — the group-commit window shows up there
and in Table 1/2 when run with `CONC=8`.

## Design notes

- Window must be ≤ 1 ms to avoid adding visible latency for single requests.
  100 µs is the right default: enough to catch same-burst commits, invisible to
  a human timing a query.
- If only one commit arrives in the window, fsync proceeds immediately — no
  latency added on a quiet server.
- `UNIDB_GROUP_COMMIT_WINDOW_US=0` (default) keeps existing behaviour exactly.
  The feature opt-in allows the benchmark to be run fairly (compare with/without).
- No `FORMAT_VERSION` bump — purely a runtime behaviour change in the WAL sealer.
- No crash-recovery change — recovery reads WAL records in order regardless of
  how many were flushed in one fsync.

## Acceptance criteria

- `UNIDB_GROUP_COMMIT_WINDOW_US=100` active: 8-concurrent INSERT benchmark shows
  ≥ 1.5× improvement vs `=0` at 10k rows total.
- Single-session INSERT throughput unchanged (≤ 5% noise vs baseline).
- All 50 crash tests green — D5 invariant maintained.
- `PUT /config/group_commit_window_us` accepted and takes effect immediately.

## Dependencies

**None — can start immediately, parallel to items 67-92.**
- `wal.rs` is NOT touched by items 67-92.
- `lib.rs` `commit()` is NOT touched by items 67-92 (they touch `hnsw_worker`
  and `exec_insert`, different code paths).
- Safe to develop in a fresh worktree off `origin/main` now.
