# Parallel scan workers (Postgres-style) — design doc

**Type:** Milestone
**Status:** SHIPPED (→ PROGRESS.md "Milestone P — parallel scan workers")

## Status as of 2026-07-10: **SHIPPED** (P-a + P-b), branch `parallel-scan`

Shipped 2026-07-10 — see `PROGRESS.md`'s "Milestone P — parallel scan workers"
entry. **The gating "correctness landmine" below turned out not to exist**: unidb
is mmap-as-storage (owned-copy reads under the mmap read-lock always see current
committed data), so parallel scan was clean to build. Results: unfiltered
`SELECT COUNT(*)` **3.82×** (now ~5–8× faster than Postgres); filtered
`COUNT(*) WHERE …` **6.6×** via **partial aggregate** (PG lead +540% → +82%).

**Partial aggregate — DONE:** the filtered `COUNT(*) WHERE <predicate>` case (was
the Amdahl-limited 1.59× "base scan only") now pushes scan + filter + count all
into the workers — `parallel_count_matching` + `QExpr::has_subquery` (a
subquery-free predicate evaluates via the pure `eval_qexpr`; subquery predicates
fall back). Result: **6.6×** at 1M rows.

**Parallel filtered `SELECT` — DONE (2026-07-11, branch `parallel-index-select`):**
a filtered `SELECT … WHERE k …` (was ~0.14× vs PG, the worst ÷PG in the suite) is
served by the B-tree index-candidate path (`try_exec_select_btree`), which
resolved candidates serially. `parallel_resolve_candidates` partitions the
candidate `RowId` list across workers (`heap::get_visible` + the B2 per-row
closure). Result: **6.41×** at 500k rows.

**Worker governance + default-ON — DONE (2026-07-11, item 15,
`15_parallel_worker_governance.md`):** parallel scan shipped **default-off**
because it lacked a global worker cap and didn't propagate timeout/cancellation
into workers. Item 15 added a process-wide worker budget (`WorkerLease` RAII
admission — total live workers never exceed `UNIDB_PARALLEL_MAX_TOTAL_WORKERS`,
extra queries degrade to serial) and a `snapshot_deadline()` that workers check
every few pages (`QueryTimeout`/`QueryCancelled`), then **flipped it default-ON**
(`ENABLED = true`; `UNIDB_PARALLEL_SCAN=0` / `set_parallel_scan(false)` remain the
field revert). This is also why `report.sh` previously showed no parallel win —
the bench never set the toggle, so it ran serial; default-on the bench now shows
it (Table 3.1 @1M ~5.6M → **~35.7M rec/s**).

**Filed follow-ups (not yet done):**
- `SUM`/`AVG`/`GROUP BY` partial aggregate (only `COUNT(*)` is pushed into workers
  so far — needs per-worker partial states + a gather-merge).
- `LIMIT` early-stop across workers (shared done-flag).
- `exec_select_readonly` (server `ReadHandle`) parallelism — its reader is a
  generic `P: PageReader`; needs a `SharedPageReader`-specific path.
- A visibility-map fast count (the true COUNT accelerator; a storage feature).

---

## Original design doc (below, as filed during Phase B)

## Why

The scan-throughput gap at scale (Table 3.1 `COUNT`/`SELECT-all` ~8× behind
Postgres at 1–2M rows) **is Postgres's parallelism** — a single-threaded scan
cannot close it, and decode pushdown (Phase B) only cuts per-row CPU, not scan
parallelism. This is the lever that makes that gap removable.

## Feasibility (favorable)

- `Engine` is `Send + Sync` (Phase 5); the mmap read path is concurrent
  (`Arc<RwLock>`, P5.a); off-thread snapshot reads are already proven by
  `read_handle.rs` (6b). A read runs under a **fixed MVCC snapshot**, so workers
  need no coordination.
- Uses **`std::thread::scope`** (NOT tokio — §4 sync-core invariant), matching the
  autovacuum / index-worker precedent.
- **Read-only** → no crash/recovery/format change; the crash-harness count is
  unchanged.

## The gating correctness question (solve first)

unidb splits authoritative state between **buffer-pool frames (dirty, current)**
and the **mmap (may be stale)**. A worker reading the *raw mmap* can read a
pre-modification page for a committed-but-unflushed row → **wrong results**.
Workers MUST use the same reconciling, pool-aware read path `ReadHandle` uses,
**not** a raw mmap read. Confirm/extend that path to N concurrent readers before
building anything on top.

## Design (once the read path is confirmed pool-consistent)

- **Dynamic block assignment**: workers grab the next page from a shared
  `AtomicUsize` cursor — **not** static `page_ids` slices, which skew on
  visible-row density / tuple width / match rate (the PG parallel-seqscan lesson).
- One **shared MVCC snapshot + one `ReadRegistration`** for the whole scan (holds
  the vacuum horizon correctly, M10).
- Per-worker: B2 selective decode + predicate + a **partial aggregate**; a
  **gather** step combines — `COUNT`=Σ, `SUM`=Σ, `GROUP BY`=merge partial hash
  maps, plain `SELECT`=concat.
- **Cost gate**: parallelize only above a page-count threshold (small scans keep
  the serial fast path); degree from cores, capped/configurable
  (`UNIDB_MAX_PARALLEL_WORKERS`). Never inside the writer path; never per-row
  (correlated subqueries stay serial).
- **`LIMIT` early-stop** across workers via a shared done-flag.
- **Peak RSS**: N partial batches — must be bounded and reported.
- **Fair benchmarking**: unidb-parallel vs Postgres-parallel (both parallel), and
  report the serial number too so the parallel speedup is isolated.

## Verification (when built)
- Differential-vs-SQLite unchanged (result order is nondeterministic without
  `ORDER BY` — compare as sets).
- `cargo test --test crash` unchanged (read-only).
- Table 3.1 `COUNT`/`SELECT-all` at 1–2M: parallel speedup toward `≤ ~2×` of
  Postgres; report serial + parallel + peak RSS.
