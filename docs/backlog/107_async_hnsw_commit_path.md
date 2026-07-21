# 107 — Async HNSW maintenance on the commit path (restore W4 ≈ W0)

**Type:** Performance
**Status:** ⏳ NOT STARTED — filed 2026-07-21 from the consolidated bench
(`docs/performance/report_20260721_035629.md`)

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
