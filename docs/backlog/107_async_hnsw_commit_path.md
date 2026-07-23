# 107 — Async HNSW maintenance on the commit path (restore W4 ≈ W0)

**Type:** Performance
**Status:** ✅ SHIPPED 2026-07-22 (PR #196, merged) — Step-0 audit found the
item-67 async worker existed but nothing spawned it (server and bench both
took the sync fallback — the source of the measured W4/W0 96×);
`EngineHandle::spawn` now activates it on served engines. Freshness contract
**(a)** signed off by user 2026-07-22 (documented bounded lag + queue-depth
gauge `unidb_hnsw_queue_depth`; hybrid tail-scan rejected for now); bench
drain-accounting added. Remaining follow-up: fresh full Docker bench on main
to officially record the W4/W0 ladder collapse (tracked in `backlog_index.md`
"Next up"). See PROGRESS.md "Item 107".

## Problem — the headline multi-model thesis is currently broken

The W0→W4 ladder's thesis is "one shared fsync makes W4 ≈ W0". The
2026-07-21 full bench measures:

| rows | W0 | Δ vector (W2−W1) | W4 | W4/W0 |
|-----:|---:|---:|---:|---:|
| 1000 | 0.43 ms | **+6.62 ms** | 8.38 ms | 19.5× |
| 10000 | 0.44 ms | **+7.72 ms** | 7.66 ms | 17.6× |
| 100000 | 0.23 ms | **+17.55 ms** | 21.79 ms | 96.0× |

Table 4 (multi-model vs replaced stack) shows the same: 13.4 ms/txn at 100k
= 0.01× the PG-relational floor. The entire blowup is the **synchronous
incremental HNSW insert** (beam search per row) inside the commit. The old
baseline's W4/W0 ≈ 1.5× dates from the IVF era (item 62 and earlier), where
vector maintenance was a cheap posting-list append; item 63's disk-HNSW
switch bought query speed (item 92: warm NEAR ~900 µs) at per-commit insert
cost. Meanwhile W0 itself improved (item 104 fsync dedup: 0.23 ms at 100k),
widening the ratio further.

**This is not a new design question.** CLAUDE.md §5 M2 already specifies:
*"HNSW secondary index built **asynchronously** in a background worker (row
write is the only synchronous cost)."* The current per-commit synchronous
maintenance contradicts the locked milestone design; this item implements
what M2 prescribed.

## Step 0 (mandatory before building)

Audit what item 67 ("async HNSW", PR #171) actually made async — build-time
backfill vs per-commit maintenance — and measure the current per-commit HNSW
path in isolation (the ladder's W2 rung is the harness). Confirm the queue /
worker infrastructure available (item 20 Dispatcher, item 26 EventWake) and
pick the mechanism.

## Sketch (to be validated in Step 0)

- Commit path enqueues (row_id, vector) — the row write + WAL append remain
  the only synchronous cost; HNSW graph insert happens in a background
  worker (same shape as the M2 design and the item-63 build worker).
- NEAR queries over-fetch-then-filter already tolerate an index that lags
  the heap: candidates are MVCC-checked against the heap (`heap.get` +
  visibility), and a **not-yet-indexed** committed row must still be
  reachable — decide the freshness contract (e.g. brute-force scan of the
  unindexed tail, or bounded lag + read-your-writes via tail scan) BEFORE
  coding; this is the correctness crux.
- Crash contract: index is rebuildable/crash-consistent from committed heap
  (the item-63 property); the queue itself need not be durable if the
  worker reconciles index-vs-heap on open.

## Targets

- W4/W0 ≤ 3× at 100k rows (ladder), Table 4 unidb ≥ 0.5× PG-relational
  floor at 100k, with NEAR recall/latency gates (item 92's ≤1 ms, recall
  ≥0.90) unchanged and the freshness contract documented + tested.
- Crash harness: new injection point around worker/queue reconciliation.

## Acceptance criteria

- [ ] Step-0 audit + isolated W2-rung measurement recorded before coding.
- [ ] Freshness contract decided, documented, and tested (including
      read-your-writes behavior inside an explicit transaction).
- [ ] W4/W0 and Table 4 targets met in a full Docker bench.
- [ ] Crash harness green including the new reconciliation point.
- [ ] No NEAR latency/recall regression (perf_item92 10k gates).


## Step-0 audit result (2026-07-22) — the worker already existed

Item 67 (PR #171) already implemented the per-commit async worker end to end:
`spawn_hnsw_worker` (bounded 4,096-slot `sync_channel`, natural backpressure
via blocking `send`), executor dispatch in `exec_insert`, `wait_hnsw_idle`,
crash contract tested (heap survives; index may lag). **But nothing spawned
it**: activation happens only in `Engine::open_arc`, while BOTH the
production server (`EngineHandle::spawn` → bare `Engine::open`) and the bench
(`bench_engine_open` → `Engine::open_with_pool_capacity`, sometimes
`Arc::new`-wrapped after the fact) took the synchronous fallback. The 96×
W4/W0 measured the fallback path.

## Freshness contract — signed off (a)

NEAR may miss rows committed in the last instants; the lag is the queue
depth: ~8–18 ms per row when idle (one background insert), worst case
~30–70 s at a saturated 4,096 queue, then backpressure blocks inserters so
it cannot grow further. Every non-NEAR read sees committed rows immediately
(MVCC untouched). The lag is observable: `Engine::hnsw_queue_depth()` +
`unidb_hnsw_queue_depth` gauge on `/metrics`. The hybrid tail-scan
(read-your-writes, ~150–300 µs worst-case per NEAR) was presented and
declined — re-open only with a user request.

## Implemented (this branch)

- `EngineHandle::spawn` now calls `engine.spawn_hnsw_worker()` — the served
  engine takes the async path (the point of the item).
- Queue-depth gauge: `HNSW_QUEUE_DEPTH` (inc on successful enqueue, dec on
  worker apply) + `HNSW_WORKER_APPLIED` counter; `Engine::hnsw_queue_depth()`;
  `unidb_hnsw_queue_depth` on `/metrics`. Enqueue failure (worker gone at
  teardown) now falls back to the synchronous insert instead of silently
  skipping the index.
- Bench honesty: `bench_engine_open_arc` (worker on) used by the W-ladder
  and Table 4; ladder reports commit latency AND a separate per-commit
  **drain** table (deferred work is not eliminated work); Table 4's timed
  window ends after `wait_hnsw_idle` so sustained throughput includes the
  worker's cost.
- Test: `item107_queue_depth_gauge_drains_to_zero`.

## Expected shape of the next report (honest prediction)

Ladder W2–W4 *commit* rows collapse toward W0 (target W4/W0 ≤ 3×), with the
deferred cost visible in the new drain table instead of hidden. Table 4
sustained throughput improves only modestly (backpressure bounds it by
worker speed — single worker ≈ the old sync rate at saturation); the honest
sustained-ingest lever remains faster HNSW insert (item 65 residue / item
106 adjacency).
