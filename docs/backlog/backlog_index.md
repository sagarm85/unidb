# Backlog index

> The **single at-a-glance registry** of every backlog effort — its number, type,
> and status (pending vs completed) — plus what's planned next. Naming & lifecycle
> rules: [`CONVENTIONS.md`](CONVENTIONS.md). Shipped metrics: `PROGRESS.md`.
>
> **The number is a stable ID** (assigned once, never renumbered — links stay
> valid). **Existing files keep their names**; every **new** backlog file is named
> `NN_<slug>.md` where `NN` is its number here. **Next new file → `18_…`.**
> "What to do next" is the **Next up** section below (reorder freely — priority is
> not the ID).

## Registry

| # | file | type | status |
|--:|------|------|--------|
| 01 | `phase1_acid_hardening.md` | Phase | ✅ SHIPPED (PROGRESS: Phase 1) |
| 02 | `phase2_data_model.md` | Phase | ✅ SHIPPED (PROGRESS: P2.a–P2.e) |
| 03 | `phase3_durable_storage.md` | Phase | ✅ SHIPPED (PROGRESS: Phase 3) |
| 04 | `phase4_query_power.md` | Phase | ✅ SHIPPED (PROGRESS: Phase 4) |
| 05 | `phase5_concurrency.md` | Phase | ✅ SHIPPED (PROGRESS: Phase 5) |
| 06 | `phase6_ops_ha.md` | Phase | ✅ SHIPPED (PROGRESS: Phase 6) |
| 07 | `commit_time_fsync.md` | Improvement | ✅ SHIPPED (PROGRESS: Commit-time WAL fsync) |
| 08 | `pg_baseline_comparison.md` | Performance | ✅ SHIPPED (PROGRESS: Postgres baseline comparison) |
| 09 | `autovacuum.md` | Improvement | ✅ SHIPPED (PROGRESS: Autovacuum) |
| 10 | `durable_fsm_catalog_pagelist.md` | Improvement | ✅ SHIPPED (PROGRESS: Durable on-disk FSM) |
| 11 | `index_write_concurrency.md` | Improvement | ✅ SHIPPED (PROGRESS: Index & heap write concurrency) |
| 12 | `rest_api_enrichment.md` | Improvement | ✅ SHIPPED (PROGRESS: REST API enrichment) |
| 13 | `crud_performance.md` | Performance | ✅ SHIPPED (PROGRESS: CRUD performance — Phase A + B) |
| 14 | `parallel_scan.md` | Milestone | ✅ SHIPPED (PROGRESS: Milestone P + follow-ups) |
| 15 | `15_parallel_worker_governance.md` | Improvement | ✅ SHIPPED (PROGRESS: Parallel worker governance) |
| 16 | `16_concurrent_sql_writes_visibility_anomaly.md` | Improvement | ⬜ NOT STARTED (reserved — see Next up) |
| 17 | `17_mm_replaced_stack_headline.md` | Performance | ✅ SHIPPED (PROGRESS: Cross-domain headline vs replaced stack) |

Meta docs (not numbered work items): `roadmap.md` (the numbered-phase plan),
`CONVENTIONS.md` (this standard), `engine_internals_doc_prompt.md` (tooling).

## Next up (candidates — pick one, then create `NN_<slug>.md`)

Ordered by my current ROI read; reorder as priorities change. None has its own
file yet — each is *filed inside* an existing doc until started.

1. **`16_concurrent_sql_writes_visibility_anomaly.md` — pre-existing MVCC
   anomaly under `UNIDB_CONCURRENT_SQL_WRITES`** (found 2026-07-11 while
   verifying item 12, NOT caused by it — reproduced on unmodified `main`):
   under CPU contention, `tests/concurrent_writers.rs::
   cross_row_update_deadlock_resolves_no_hang` intermittently ends with
   **3 visible rows instead of 2** after cross-row UPDATE churn on an indexed
   table (a superseded/aborted version stays visible). Repro: run the
   `concurrent_writers` test binary 6× in parallel, filter `cross_row` —
   fails ~1–5 of 6 instances per round on `main` (dc93931) and on the item-12
   branch alike; passes in isolation. Correctness bug in item 11's
   default-OFF toggle path (the test enables it explicitly) — must be
   root-caused before that toggle's planned default-ON flip. Filed in
   `index_write_concurrency.md`'s follow-ups; the toggle stays default-off.
2. **A2 / HOT-style update — DEFERRED (ROI vs §1), not filed.** Would reopen
   locked decision D4 (`FORMAT_VERSION` bump) + recovery + new crash points for a
   ~0.34× → ~0.42× UPDATE-bulk gain on a **single-model** CRUD bench that §1 says
   we should lose anyway. Not worth a locked-decision change; effort redirected to
   #17 (the §6 cross-domain headline). Filed rationale in `crud_performance.md`; if
   ever picked up it takes the next free number (`18_…`).
3. **Parallel-scan follow-ups** (filed in `parallel_scan.md`, lower ROI):
   `SUM`/`AVG`/`GROUP BY` partial aggregate; `LIMIT` early-stop; server
   `ReadHandle` parallelism; a visibility-map fast count. (Default-on + worker
   governance already shipped as #15.)
4. **Attach-client session support** (filed in `rest_api_enrichment.md`,
   shipped item 12's one optional follow-up): wrap `X-Txn-Id` sessions +
   `/rows/batch` + cursors in `unidb-attach`.

## How to update this file

- **Start** an item → set status to 🔄 IN PROGRESS; if it's a "Next up"
  candidate, create its `NN_<slug>.md` (next free number) and add a Registry row.
- **Ship** it → status → ✅ SHIPPED with the `PROGRESS.md` entry name.
- Keep this the source of truth for *what exists and where it stands*; keep
  metrics in `PROGRESS.md` and running state in `MEMORY.md`.
