# 10. Parallelism & Performance — Mechanisms and Metrics Analysis

**Modules:** `sql/parallel_scan.rs`, `wal.rs` (group commit), `btree_index.rs`
(crabbing), `lib.rs` (catalog routing), `benches/*`.
The engine core stays **synchronous** — every parallel mechanism below is
`std::thread`, never tokio.

---

## 10.1 Write parallelism

Three stacked mechanisms, each independently toggleable/observable:

1. **Group commit (P5.e-4).** Commit durability goes through
   `Wal::sync_up_to(commit_lsn)` with leader election on a dedicated flush lock;
   the leader runs `sync_all` **with the append lock released**, so concurrent
   committers keep appending and one fsync covers them all. Embedded scaling:
   1→325, 4→647, **8→1,197 commits/s (3.68×)**. Server-side: `POST /sql` INSERT
   throughput went from flat (~135–158 ops/s at any concurrency) to **242 / 756
   / 4,780 ops/s at 1/10/50 clients (31×)**.
2. **Heap-level concurrency (P5.e).** `Engine` is `Send + Sync`; per-page
   exclusive latches make every heap read-modify-write atomic; the FSM mutex is
   never held across latch/WAL work; a coarse `write_serial` mutex covers only
   the structural paths (DDL, vacuum, edges/LOBs/events enablement). Raw CRUD
   and reads are fully concurrent.
3. **Concurrent SQL writes (default-off toggle `UNIDB_CONCURRENT_SQL_WRITES`).**
   `CatalogHandle::{Shared, Exclusive}` routes catalog-non-mutating DML
   (SELECT / INSERT-into-FSM-backed-non-SERIAL / UPDATE / DELETE) through a
   shared catalog read lock; DDL and catalog-mutating DML take the write lock.
   Under the shared lock, only the storage layer serializes writers: page
   latches + **B-tree latch-crabbing** (doc 6 §3). Measured: indexed 8-writer
   768 → **1,058 commits/s (+38 %)**, near the ~1,260 unindexed fsync floor.
   Toggle-off reproduces old behavior byte-for-byte — the field revert switch.

Supporting result: the "SQL path won't scale because of the catalog RwLock"
prediction was **refuted by measurement** — 8-writer SQL scaling hit 3.82×
(vs Postgres 3.81×) even *before* the shared-catalog work, because the lock
covers only fast in-memory execution while the dominant fsync coalesces outside
it. Evidence over intuition is the house rule (`CLAUDE.md §6`).

## 10.2 Read parallelism — the parallel scan engine (Milestone P)

```mermaid
flowchart TB
    subgraph Setup
        Q["query_exec routing"] --> D{"degree_for(n_pages)<br/>enabled? · ≥ MIN_PAGES(64)? · cores?"}
        D -->|None| SER["serial path"]
        D -->|Some(n)| SPAWN["std::thread::scope, n workers"]
    end
    subgraph Workers["shared: mmap (read lock) · one immutable Snapshot · AtomicUsize page cursor"]
        W1["worker: i = cursor.fetch_add(1)<br/>process pages[i] … repeat"]
        W2["worker …"]
    end
    SPAWN --> W1 & W2
    W1 & W2 --> G{gather}
    G -->|counts| SUM["atomic sum"]
    G -->|rows| CAT["concat (order-agnostic)"]
```

- **Dynamic block assignment** — a shared atomic page cursor, not static slices —
  deliberately avoids the straggler skew of static partitioning.
- **Correctness is inherited, not bolted on.** unidb is *mmap-as-storage*:
  writers mutate the mmap under its write lock; a worker reads an **owned page
  copy** under the read lock (torn-read-free) and filters with the same
  immutable statement snapshot + `is_visible` as the serial path — so a parallel
  result is a valid point-in-time answer even under a concurrent writer
  (tested: parallel-vs-serial equality, MVCC honoring, concurrent-writer
  stress). A feared "pool newer than mmap" hazard was investigated and shown
  **not to exist** in this architecture — that's a Postgres-shaped problem.
- **Four primitives**, routed by shape:
  | Primitive | Used for | Gather |
  |---|---|---|
  | `parallel_count` | unfiltered `COUNT(*)` (header-only) | atomic sum |
  | `parallel_count_matching` | filtered `COUNT(*)` — **partial aggregate**: scan→filter→count entirely inside workers | sum |
  | `parallel_filter_project` | full-scan SELECT | concat + RowIds (SSI read set) |
  | `parallel_resolve_candidates` | B-tree candidate resolution (filtered SELECT hot path) | concat |
- **Fallbacks:** subquery predicates run serial (they need executor storage
  access); small tables (< 64 pages) stay serial. Read-only, no format change,
  crash harness untouched.
- **Governance (backlog item 15, shipped 2026-07-11) — and default-ON.** Two
  blockers were closed and the toggle flipped on by default:
  - **Global worker cap:** a process-wide worker budget with `WorkerLease` RAII
    admission (CAS-takes `min(degree, available)`, releases on Drop, < 2 →
    serial) — total live workers never exceed the cap across all concurrent
    queries, eliminating M×N oversubscription
    (`UNIDB_PARALLEL_MAX_TOTAL_WORKERS` / `set_parallel_scan_max_total_workers`).
  - **Timeout/cancellation:** workers snapshot a `Send + Sync` deadline +
    cancel token and check every few pages — a runaway parallel scan is
    interruptible exactly like the serial path.
  - `UNIDB_PARALLEL_SCAN=0` / `set_parallel_scan(false)` remain the field
    revert. Default-on also fixed a benchmark trap: `report.sh` previously
    showed "no parallel win" because it never set the env var and measured the
    serial path (Table 3.1 @1 M scan: 5.6 M → **35.7 M rows/s** with no env).

Measured (Apple M5 Pro, 18 cores, 1 M rows):

| Workload | Serial | Parallel | Speedup | vs Postgres |
|---|---|---|---|---|
| `SELECT COUNT(*)` | 77.2 M rows/s | **294.9 M rows/s** | **3.82×** | ~5–8× **faster** |
| `COUNT(*) WHERE …` (partial agg) | 5.37 M | **35.4 M** | **6.6×** | 0.16× → **0.55×** |
| Filtered `SELECT` via index candidates (500 k rows) | 995 k | **6.4 M** | **6.41×** | closed the worst ÷PG gap |

The filtered-count history is instructive: parallelizing only the base scan gave
1.59×; pushing the whole scan→filter→count into workers (partial aggregate) gave
6.6×. Amdahl's law is the design guide — the serial tail must move into the
workers, not just the scan.

## 10.3 Read-path (serial) wins recap

Covered in doc 5 §4; the compounding sequence that preceded parallelism:

- decode pushdown (dec/row 2.00 → 0.00, +28 % filtered SELECT),
- header-only COUNT (2.81× faster than Postgres),
- candidate `(page, slot)` ordering,
- index-WAL coalescing on the write path (UPDATE bulk 3.3×, WAL 14× smaller).

## 10.4 Benchmark & metrics analysis

**Method** (`CLAUDE.md §6`, non-negotiable): single-model numbers are compared
against **SQLite** (the honest embedded analog) and, as a fitness check, against
native **Postgres 18.4 at matched true durability** — macOS lens 2
(`fsync_writethrough` = F_FULLFSYNC) — never the lens-1 illusion where plain
`fsync()` doesn't reach the platter (a 40× phantom "advantage"). The Docker path
(`scripts/report.sh`) reruns everything on Linux where both engines share plain
`fsync`. Every milestone PR records throughput, p50/p99, and **peak RSS** in
`PROGRESS.md`. All numbers below: Apple M5 Pro (18 cores), release builds.

### Durable commit ladder (the multi-model headline)

| Rung | ms/commit | Baseline |
|---|---|---|
| W0 plain row | **3.59** | SQLite 3.64 (WAL + FULL + fullfsync) — **parity** |
| W1 + B-tree | 4.39 | SQLite+index 4.03 |
| W2 + VECTOR(128) IVF | 4.36 | — |
| W3 + graph edge | 4.24 | — |
| **W4 + event (full multi-model)** | **4.40** | replaced-stack: 3–4 systems, no shared txn |

Pre-flip (per-statement fsync) W4 was ~33.1 ms — the commit-time-fsync default
bought **~7.5×**. `w4_1fsync` (hand-built single-fsync rung) matches W4 within
noise — proof the flip landed.

### vs Postgres (lens 2, matched durability)

| Metric | unidb | Postgres | Verdict |
|---|---|---|---|
| Durable single-row INSERT | 3.58 ms | 3.31 ms | parity (both fsync-bound) |
| Point SELECT | **6.87 µs** | 33.6 µs | **unidb 4.9×** |
| Point read across 10 k→1 M rows | flat 3.2–5.3 µs | 61–69 µs | **~13× at every size; nothing bends** |
| MVCC UPDATE (single) | 4.00 ms | 3.65 ms | PG +10 % |
| UPDATE bulk (post-Phase-A) | 0.34× PG | — | honest gap; HOT updates are the path to parity |
| Read after 30× churn | 35.4 µs → **5.85 µs after vacuum** | ~35 µs steady | automation (autovacuum, now shipped) not capability |
| 8-writer commits/s | 1,121 raw / 1,205 SQL | 1,179 | **both scale 3.5–3.8×** |
| Peak RSS | ~18–35 MB | server-class | embedded footprint |

### Efficiency counters (the "measure, don't assume" layer)

`wal_total_bytes_appended` (WAL B/row), `ROWS_DECODED`/`COLS_DECODED` (dec/row,
cols/row) are first-class bench columns — each optimization above was accepted
on a counter delta plus a wall-clock delta, and each records its *residual*
(e.g. UPDATE's remaining gap is insert-new-version MVCC cost, reachable only via
HOT — filed, not hand-waved).

### Honest asymmetries (stated, not buried)

- Docker Desktop's VM fsync is not flush-to-platter — ratios fair, absolutes
  VM-bound; publish native-Linux numbers.
- Table-3.1 large-scan losses were PG's *parallel* seq-scan vs unidb's then
  single-threaded scan — a real capability gap, since addressed by Milestone P.
- `COUNT(*)` wins partly reflect PG's visibility-map/analyze overheads at small
  scale; the O(pages) ceiling is documented.

## 10.5 Border cases

| Case | Handling |
|---|---|
| Parallel worker hits an error | error funnel (first error wins) + atomic stop flag; scan aborts cleanly |
| Worker vs concurrent writer | owned page copies under mmap read lock — torn-read-free (tested) |
| SSI under parallel scan | workers return RowIds; read set noted exactly as serial |
| Subquery in predicate | serial fallback |
| Tiny table | `MIN_PAGES` threshold keeps thread cost off small scans |
| Many concurrent parallel queries | global worker cap + `WorkerLease` admission — no M×N thread explosion; over-budget queries degrade to serial |
| Runaway/cancelled parallel scan | per-worker deadline + cancel-token checks every few pages |
| Toggle flipped mid-flight | atomics read per statement; no reopen needed |
| Group-commit leader's fsync fails | poisoning (doc 3 §2) — followers get `DurabilityFailure`, never false success |
