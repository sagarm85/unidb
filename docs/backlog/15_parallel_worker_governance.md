# Parallel worker governance

**Type:** Improvement
**Status:** IN PROGRESS (branch `parallel-worker-governance`)

## Context

Parallel scan (Milestone P, `parallel_scan.md`) is **correct** — MVCC-snapshot +
owned-copy mmap reads; the 373-test suite passes with it forced on; it matches
serial, honors MVCC, and is torn-read-free under a concurrent writer. But it
ships **default-off** because its **resource governance under concurrent load is
immature**, and those gaps (verified in code) are the real blockers to
default-on:

1. **No global worker cap.** Each query independently spawns up to its own
   `degree` — no ceiling across concurrent queries. M concurrent large scans × N
   workers = **M×N threads** → oversubscription, context-switch thrash, aggregate
   throughput *worse* than serial. (Postgres gates this with a global
   `max_parallel_workers` pool.)
2. **Default degree = all cores, per query** — one scan monopolizes the box.
3. **Timeout/cancellation not propagated to workers** — the serial path checks
   `query_limits` every 1024 rows; workers never do, so a runaway parallel scan
   won't honor a query deadline or `CancelToken`.
4. No thread pool (fresh `std::thread::scope` spawn per scan — minor).

This closes 1–3 (the *safety* blockers), load-tests under concurrency, then flips
the toggle **default-on** (the runtime toggle stays as the field-revert net).
Read-only feature → crash harness unchanged, no `FORMAT_VERSION` bump, no §3.

## Approach

### G1 — Global worker admission (the safety net)
- A process-wide budget: `static AVAILABLE: AtomicUsize` (init = global cap,
  default `available_parallelism`), plus `GLOBAL_MAX` (env
  `UNIDB_PARALLEL_MAX_TOTAL_WORKERS`).
- **`WorkerLease` (RAII)**: a query asks for its per-query `degree`; `acquire`
  atomically takes `min(degree, AVAILABLE)` via CAS; **releases on `Drop`** (so
  permits come back even on an early `?` error). If the grant is `< 2`, release
  and return `None` → the caller runs its existing **serial** path.
- Replace the call sites' `degree_for(n)` gate with `acquire(n) -> Option<WorkerLease>`;
  pass `lease.degree()` as the worker count. `parallel_*` functions are otherwise
  unchanged. Net effect: **total live parallel-scan workers never exceed
  `GLOBAL_MAX`**, no matter how many queries run at once; extra queries degrade to
  serial instead of oversubscribing.

### G2 — Timeout / cancellation propagation
- `query_limits`: expose the calling thread's `(deadline: Option<Instant>,
  cancel: Option<CancelToken>)` snapshot (both `Send + Sync`).
- Pass them into the workers; each checks **every K pages/candidates**: past the
  deadline → `DbError::QueryTimeout`; `cancel.is_cancelled()` →
  `DbError::QueryCancelled`; set the shared `stop` flag + record the error (same
  path as a worker error today). A runaway/large parallel scan is now interruptible
  exactly like the serial scan.

### G3 — Concurrency load-test
- `tests/parallel_scan.rs`: launch **M concurrent parallel scans** on one
  `Arc<Engine>` and assert (a) every result is correct, (b) with `GLOBAL_MAX`
  set small, the work still completes and stays bounded (no thread blow-up /
  deadlock), (c) a `CancelToken` / short deadline interrupts an in-flight parallel
  scan promptly.

### G4 — Flip default-on + let the bench show it
- With G1–G3 in place the safety blockers are gone, so flip
  `ENABLED = AtomicBool::new(true)` (the runtime toggle + `UNIDB_PARALLEL_SCAN=0`
  remain the field-revert net; per-query + global caps bound the cost). The
  `decompose` bench / `report.sh` then reflect the parallel numbers **by default**
  (this is the fix for "report.sh shows no improvement").
- Report note: state that parallel scan is on, capped by `GLOBAL_MAX`.

## Files
- `src/sql/parallel_scan.rs` — `WorkerLease` + `acquire`/global budget; deadline/
  cancel checks in the four workers; `ENABLED` default → true.
- `src/query_limits.rs` — a `Send+Sync` snapshot accessor for deadline + cancel.
- `src/sql/executor.rs`, `src/sql/query_exec.rs` — call sites use `acquire()` +
  `lease.degree()` instead of `degree_for()`.
- `src/lib.rs` — extend `set_parallel_scan_config` (or a new setter) for the
  global cap.
- `tests/parallel_scan.rs` — concurrency + cancellation load-tests.

## Verification
- New load-tests green; existing parallel tests green; **full lib (373) green**
  (default-on now — the suite runs the parallel path by default); `crash` stays
  **29**; `clippy -D warnings` + `fmt` clean; `cargo tree` tokio-free.
- Benchmark: `report.sh` (or `decompose` mmreport) now shows the parallel scan
  numbers by default (Table 3.1 @1M ~5.6M → ~35M rec/s); a concurrency micro-run
  shows aggregate throughput does **not** collapse (bounded by `GLOBAL_MAX`).
- Close out: `PROGRESS.md` entry, `MEMORY.md`, flip this file → SHIPPED,
  `parallel_scan.md` follow-ups updated, `README.md` backlog index (#15 →
  SHIPPED), engine_design note.

## Non-goals (still deferred)
A real thread **pool** (spawn reuse) — minor overhead, not a safety issue; a
work-stealing scheduler; `SUM`/`GROUP BY` partial aggregate; `LIMIT` early-stop.
