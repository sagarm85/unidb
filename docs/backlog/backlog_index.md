# Backlog index

> The **single at-a-glance registry** of every backlog effort — its number, type,
> and status (pending vs completed) — plus what's planned next. Naming & lifecycle
> rules: [`CONVENTIONS.md`](CONVENTIONS.md). Shipped metrics: `PROGRESS.md`.
>
> **The number is a stable ID** (assigned once, never renumbered — links stay
> valid). **Existing files keep their names**; every **new** backlog file is named
> `NN_<slug>.md` where `NN` is its number here. **Next new file → `19_…`.**
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
| 16 | `16_concurrent_sql_writes_visibility_anomaly.md` | Improvement | ✅ SHIPPED (PROGRESS: MVCC visibility anomaly under concurrent SQL writes) |
| 17 | `17_mm_replaced_stack_headline.md` | Performance | ✅ SHIPPED (PROGRESS: Cross-domain headline vs replaced stack) |
| 18 | `18_engine_access_contract.md` | Milestone | ⏳ NOT STARTED |

Meta docs (not numbered work items): `roadmap.md` (the numbered-phase plan),
`CONVENTIONS.md` (this standard), `engine_internals_doc_prompt.md` (tooling).

## Next up (candidates — pick one, then create `NN_<slug>.md`)

Ordered by my current ROI read; reorder as priorities change. Create each
candidate's `NN_<slug>.md` when started — until then each is *filed inside* an
existing doc.

0. **Item 18 — Engine access & introspection contract (`18_engine_access_contract.md`,
   already filed, NOT STARTED).** Application-driven: the `unidb-studio` console
   needs real PK/FK/DDL introspection. Deliver it as an `information_schema`-style
   **queryable catalog** + a documented access/query/type surface — *not* as new
   app REST endpoints. Core is Epic C (catalog relations for PK/FK/indexes/DDL).
   High ROI: unblocks any tool built on the engine, studio first.

1. **Item 11 `UNIDB_CONCURRENT_SQL_WRITES` default-ON flip — ✅ SHIPPED
   2026-07-13** (branch `11-concurrent-writes-default-on`). Item 16 (below)
   root-caused and fixed the soak blocker (MVCC visibility anomaly); the
   concurrency matrix passes 28/28 toggle-on **and** toggle-off at
   `CONC_REPEATS=10`. Default is now ON (`=0`/`false`/`off` forces the serialized
   fallback); Table C re-measured on the flipped default: indexed 8-writer
   **811 → 1016 commits/s** (+25%). Flip note in `index_write_concurrency.md`,
   metrics in `PROGRESS.md`. **Item 16 — MVCC visibility anomaly under
   concurrent SQL writes — is ✅ SHIPPED** (2026-07-12, branch
   `16-visibility-fix`); root cause (abort dropped the xid from `active` before
   undo), fix, and evidence live in
   `16_concurrent_sql_writes_visibility_anomaly.md`; metrics in `PROGRESS.md`.
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
