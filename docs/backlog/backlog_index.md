# Backlog index

> The **single at-a-glance registry** of every backlog effort — its number, type,
> and status (pending vs completed) — plus what's planned next. Naming & lifecycle
> rules: [`CONVENTIONS.md`](CONVENTIONS.md). Shipped metrics: `PROGRESS.md`.
>
> **The number is a stable ID** (assigned once, never renumbered — links stay
> valid). **Existing files keep their names**; every **new** backlog file is named
> `NN_<slug>.md` where `NN` is its number here. **Next new file → `51_…`.**
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
| 18 | `18_engine_access_contract.md` | Milestone | ✅ SHIPPED (PROGRESS: Engine access & introspection contract (Milestone 18)) |
| 19 | `19_sql_surface_gaps.md` | Improvement | ⏳ NOT STARTED |
| 20 | `20_events_realtime_dispatcher.md` | Milestone | ✅ SHIPPED (PROGRESS: Events / realtime dispatcher (Milestone 20)) |
| 21 | `21_observability_metrics.md` | Improvement | ✅ SHIPPED (PROGRESS: Observability metrics enrichment (item 21)) |
| 22 | `22_logs_surface.md` | Improvement | ✅ SHIPPED (PROGRESS: Logs surface — JSON structured logs, correlation ids, bounded /logs tail) |
| 23 | `23_storage_service.md` | Milestone | ✅ SHIPPED (PROGRESS: Object storage service — MinIO/S3 tiering over engine metadata (item 23)) |
| 24 | `24_authz_v2_policies.md` | Milestone | ⏳ NOT STARTED |
| 25 | `25_multipage_catalog.md` | Improvement | ✅ SHIPPED 2026-07-13 (multi-page chain; no FORMAT_VERSION bump; P33 crash point; item-23 ceiling lifted) |
| 26 | `26_event_queue_scale.md` | Improvement | ✅ SHIPPED 2026-07-13 (seq index, EventWake push, Q3 vacuum-correct) |
| 27 | `27_vacuum_per_table.md` | Improvement | ✅ SHIPPED 2026-07-13 |
| 28 | `28_replication_time_pitr_logical.md` | Milestone | ✅ SHIPPED 2026-07-13 (R1: side timeline index + restore_to_time; R2: unidb-logical crate) |
| 29 | `29_subscription_cdc_envelope_lag.md` | Improvement | ✅ SHIPPED 2026-07-13 (before/after CDC, canonical envelope, format adapters, lag observability) |
| 30 | `30_studio_api_readiness.md` | Improvement | ✅ SHIPPED 2026-07-14 (G9 LIKE/ILIKE, G11 MATCH/sql, ERP integration guide §12) |
| 31 | `31_storage_http_routes.md` | Milestone | ✅ SHIPPED 2026-07-14 (StorageApi trait + 7 /storage/* routes + 503 contract + 5 integration tests) |
| 32 | `32_bulk_load_api.md` | Performance | ✅ SHIPPED 2026-07-14 — POST /tables/{name}/bulk NDJSON endpoint; **measured ~12k–31k rows/sec** (index-dependent; ~20–50× over ~640/sec per-row); below the 50k–200k target — follow-up filed. See PROGRESS.md |
| 33 | `33_cdc_management_api.md` | Improvement | ✅ SHIPPED 2026-07-14 — `GET /tables/{name}/events` (CDC status), `DELETE /tables/{name}/events` (disable, idempotent), `GET /events/head` (current seq without streaming); P34 crash test; 6 integration tests |
| 34 | `34_observability_api_gaps.md` | Improvement | ✅ SHIPPED 2026-07-14 — `UNIDB_SLOW_QUERY_MS` env var; `PUT /config/slow_query_threshold_ms`; `GET /stats/history` 300-point ring buffer with server-computed rate fields |
| 35 | `35_unique_constraint_full_scan.md` | Improvement | ✅ SHIPPED 2026-07-14 — implicit unique-enforcement B-tree per PK/UNIQUE column at CREATE TABLE; `enforce_unique()` now does O(1) point lookup + MVCC re-check; PK INSERT flat (was O(n²)); P35 crash test; 6 regression tests; ~23-26× faster at 15k rows. See PROGRESS.md |
| 36 | `36_foreign_key_row_enforcement.md` | Improvement | ✅ SHIPPED 2026-07-14 — full row-level FK enforcement: child INSERT/UPDATE checks parent key via unique_index_root (O(log n)); parent DELETE/UPDATE RESTRICT rejects when visible child references the key; RecordKind::FkKey phantom lock prevents concurrent parent-delete/child-insert race; 9 new tests + conc_matrix cell 10/10 PASS. See PROGRESS.md |
| 37 | `37_lazy_buffer_pool_growth.md` | Improvement | ⏳ NOT STARTED — buffer pool frame table is allocated eagerly at open (`(0..capacity).map(...).collect()`), forcing one static `capacity` to serve both cheap-small-opens and generous-bulk-load goals at once. Follows up the 4096→65536 default-bump (`PROGRESS.md`); lazy/growable allocation would let a much larger ceiling be the default without taxing small/embedded opens |
| 38 | `38_param_type_coercion.md` | Improvement | ⏳ NOT STARTED — engine rejects `WHERE int_col = $1` when `$1` is bound as `Text("20")`; should coerce losslessly to the column type (standard SQL behaviour; PostgreSQL/SQLite/MySQL all do this). Studio workaround (`bindForColumn` in RecordBrowser) applied 2026-07-15 but does not cover other clients or expression contexts. |
| 39 | `39_pk_fk_relational_stress_bench.md` | Performance | ✅ SHIPPED — new Table 5 in `scripts/report.sh`'s multi-model report: a real `customers`/`orders` PK/FK schema (previously the whole report had zero `FOREIGN KEY` usage anywhere), throughput vs Postgres, plus pass/fail correctness proofs that both engines reject an invalid FK and RESTRICT a still-referenced DELETE. Made fair by item 36 (FK row-level enforcement, shipped the same day). See PROGRESS.md |

| 40 | `40_btree_bulk_build.md` | Performance | ✅ SHIPPED 2026-07-15 — sort-then-bulk-load CREATE INDEX backfill: collect (key, row_id) pairs, sort, `insert_many` (one WAL mini-txn / one fsync). 134.2 s → 12.0 s (**11.2×**) on 540k rows. P40 crash test added (38/38). See PROGRESS.md. |
| 41 | `41_near_vec_distance.md` | Improvement | ✅ SHIPPED 2026-07-14 — `exec_select_near` threads its already-computed re-rank distance through to projection as a virtual `vec_distance` column (`Literal::Float`, ascending); no catalog/format change. See PROGRESS.md. |
| 42 | `42_bench_harness_buffer_pool.md` | Improvement | ✅ SHIPPED — `benches/decompose.rs` never sized its buffer pool, so any report sweeping into 1M+ rows silently hit `BufferPoolFull` and understated unidb's real throughput (measured 1,228 rec/s vs the true 15,905 rec/s at 1M rows, ~13× recovered). New `bench_engine_open()` helper opens every bench engine with a 2,000,000-frame pool. See PROGRESS.md |
| 43 | `43_a3_gate_size_aware_selectivity.md` | Improvement | ✅ SHIPPED 2026-07-15 — size-aware cost model (`page_count > BTREE_STARTUP + matched×HEAP_FETCH_SEQ_EQUIV`), best-arm predicate selection (`find_best_indexable_btree_predicate` prefers `k<N` over `k>=0`), and A3 gate added to exec_select. Crossover at ~2600 rows for 50% selectivity; 3 permanent regression tests. PR pending. |
| 44 | `44_bulk_delete_batched_wal.md` | Performance | ✅ SHIPPED 2026-07-16 (PR #119) — `Heap::delete_many` groups already page-sorted row_ids by page_id, one WAL mini-txn per page instead of per row. WAL B/row 230 → 107 (−53%), 416k rec/s at 5000 rows. See PROGRESS.md "Items 47 + 44". |
| 45 | `45_select_filtered_parallel_btree_scan.md` | Performance | 🔄 IN PROGRESS — Lever 2 (pre-spawned worker pool, `parallel_scan.rs`) shipped 2026-07-16 (`perf/45-worker-pool`). Lever 1 (B-tree range partition, `btree_index.rs`) in progress (`perf/45b-btree-partition`). Lever 3 (arena alloc) deferred. |
| 46 | `46_select_grouped_hash_aggregate.md` | Performance | ✅ SHIPPED 2026-07-15 (PR #117) — B2 decode-pushdown extended into aggregate path (column mask to GROUP-BY exprs only); integer-keyed HashMap specialisation; DELETE-all small-candidate guard. See PROGRESS.md. |
| 47 | `47_update_delete_write_throughput.md` | Performance | ✅ SHIPPED (Phase A) 2026-07-16 (PR #119) — B-tree in-place RowId patch for unchanged-key UPDATE (`patch_many` batched across secondary + unique-enforcement indexes). WAL B/row 619 → 465 (−25% at 500-row scale). Phase B (vectorised predicate scan) and Phase C (HOT-equivalent chain) remain open follow-ons. See PROGRESS.md "Items 47 + 44". |
| 48 | `48_delete_all_truncate_fast_path.md` | Performance | ✅ SHIPPED 2026-07-15 (PR #117) — `TRUNCATE TABLE t` SQL surface + `Heap::truncate()` (single WAL record + heap/index reset); opportunistic DELETE-all → truncate routing when no FK children and no CDC subscribers. See PROGRESS.md. |
| 49 | `49_bench_pg_connect_no_timeout_hang.md` | Improvement | ✅ SHIPPED 2026-07-16 — `benches/decompose.rs` opened every Postgres connection with no `connect_timeout`; an unreachable/unresponsive `PG_URL` (wrong host, firewalled, container still starting) blocked on the OS TCP SYN-retry ceiling (~2 min/attempt, confirmed empirically) across 24 call sites with zero output — the real cause of `scripts/report.sh` reports "hanging indefinitely". New `pg_dial()` helper sets `connect_timeout` (default 10s, `PG_CONNECT_TIMEOUT_SECS`); all call sites route through it. Verified: unreachable PG_URL now fails the whole report in 14.6s instead of hanging. See PROGRESS.md. |
| 50 | `50_patch_many_infinite_loop.md` | Improvement | ✅ SHIPPED 2026-07-16 — **critical**: `DiskBTree::patch_many` (item 47) genuinely infinite-loops, single-threaded, 100% CPU, on an unchanged-key `UPDATE` whenever the very first patch in a leaf-group has a key outside that leaf's *current* `entries.first()/last()` (plausible after any split) — the bounds check gated the first entry too, so the loop index never advanced. Confirmed live via `gdb -p <pid> -batch -ex bt` (identical stack twice). This is why it was never caught: Table 3 (the only report section touching this path) only runs when Postgres is reachable, and this session's item 49 fix was the first time that condition was met. Fixed: bounds check now only gates *additional* (`j > i`) batching, never `j == i`. New permanent regression test confirmed to catch the bug pre-fix (30s hang deadline) and pass post-fix (~1s). See PROGRESS.md. |

Meta docs (not numbered work items): `roadmap.md` (the numbered-phase plan),
`CONVENTIONS.md` (this standard), `engine_internals_doc_prompt.md` (tooling).
**Next new file → `51_…`.**

## Next up (candidates — pick one, then create `NN_<slug>.md`)

Ordered by my current ROI read; reorder as priorities change. Create each
candidate's `NN_<slug>.md` when started — until then each is *filed inside* an
existing doc.

**#35 — Unique-constraint full heap scan — ✅ SHIPPED 2026-07-14.** Implicit
unique-enforcement B-tree per PK/UNIQUE column at CREATE TABLE; O(1) point
lookup + MVCC re-check in `enforce_unique()`; PK INSERT now flat at ~27-30k
rows/s (was O(n²): 5k→1k/s degrading). P35 crash test; 6 regression tests;
`unique_index_root` in `ColumnDef` with `#[serde(default)]` (no FORMAT_VERSION
bump). See PROGRESS.md.

**#36 — Foreign keys: full row-level enforcement — ✅ SHIPPED 2026-07-14.** See
`36_foreign_key_row_enforcement.md` and PROGRESS.md for details and metrics.
Child INSERT/UPDATE verifies referenced parent key via unique_index_root (O(log
n)); parent DELETE/UPDATE RESTRICT; FkKey phantom lock for concurrent-race
safety; 9 new tests + conc_matrix cell 10/10 PASS.

**#43 — A3 scan-vs-index gate is now size-aware (SHIPPED 2026-07-15,
`43_a3_gate_size_aware_selectivity.md`).**
Size-aware cost model + best-arm predicate selection + gate added to
exec_select. Crossover at ~2600 rows for 50% selectivity; 3 permanent
regression tests in `tests/a3_gate.rs`. PR pending. The remaining
large-scale performance gap vs Postgres is architectural (PG parallel index
scan vs unidb's single-threaded B-tree resolution) and is a separate
follow-up item, not item 43 scope. Original problem was:
does. Not a quick constant bump: the current 0.3 already fixes a prior
regression (forcing the index path regressed a 50%-selective DELETE) —
needs a real size-aware cost model, re-derived and measured across a size
sweep, not a single new fixed number.

**#45 — SELECT filtered remaining gap: serial B-tree scan + thread-spawn + alloc
(`45_select_filtered_parallel_btree_scan.md`, NOT STARTED).** After item 43 the gate is right (cols/row=4.00, parallel fires), but 0.35× PG remains. Three levers: (1) partitioned B-tree range across workers instead of serial candidate collection, (2) pre-spawned worker pool instead of `std::thread::scope` per query, (3) arena-allocated row data instead of per-row `Vec<Literal>` + `String`. Each is independent; (2) alone should recover most of the thread-spawn overhead.

**#46 — SELECT grouped: full-row decode + row-at-a-time hash-aggregate
(`46_select_grouped_hash_aggregate.md`, NOT STARTED).** 0.60× PG (+67%); `cols/row=4.00` shows B2 decode-pushdown not applied on the aggregate path. Extend column mask to GROUP-BY exprs only; specialize integer-key HashMap; partial-aggregate in workers.

**#47 — UPDATE/DELETE write throughput: unchanged-key B-tree patch + vectorised predicate scan
(`47_update_delete_write_throughput.md`, NOT STARTED).** Largest gaps: UPDATE 0.17×, DELETE selected 0.17×. Primary driver: `WAL B/row=619` for body-only UPDATE shows B-tree does tombstone+insert even when key is unchanged (RowId changes, key value doesn't). Phase A: in-place RowId patch in leaf node (valid when old_key==new_key). Phase B: vectorised predicate deform on `matching_rows` path. Phase C (milestone-sized): HOT-equivalent update chain.

**#48 — DELETE all / TRUNCATE fast path
(`48_delete_all_truncate_fast_path.md`, NOT STARTED).** 0.23× PG (+331%); `dec/row=1.00`, 20k per-row mini-txns for a no-predicate delete. Fix: `TRUNCATE TABLE t` + `Heap::truncate()` (single WAL record + heap+index reset); opportunistic DELETE-all routing. FK RESTRICT + CDC "truncate" event must be handled.

**#44 — Bulk DELETE pays one WAL mini-transaction per row
(`44_bulk_delete_batched_wal.md`, NOT STARTED).** `Heap::delete`
(`src/heap.rs:399`) is a self-contained mini-transaction per call
(`begin_mini_txn`/`commit_mini_txn`, its own exclusive page latch, its own
full-page-image check); `exec_delete` calls it once per matched row. A
DELETE touching N rows performs N separate WAL mini-transactions — the exact
same shape item 40 already fixed for `CREATE INDEX`. Measured: `DELETE FROM
t` (no predicate) at postgres +275%, `DELETE selected (k>=N)` at +409%
(20k rows, `multi_model_report_20260715_092725.md`). Distinct root cause from
#43 (no predicate means no scan-vs-index decision at all) — the fix is
batching deletes by page, reusing `matching_rows`'s existing physical-order
sort (B5), following item 40's precedent (N mini-txns -> num_pages
mini-txns).

**#37 — Buffer pool frame table: lazy/growable allocation
(`37_lazy_buffer_pool_growth.md`, NOT STARTED).** `BufferPool::open`
eagerly allocates `capacity` frames up front (`src/bufferpool.rs`), forcing
one static default to trade off cheap small/embedded opens against
generous headroom for large bulk loads. The default was just bumped
4096→65536 (`PROGRESS.md`, "Default buffer-pool capacity raised") after a
demo-scale seed hit `BufferPoolFull` and collapsed throughput ~15-20x via
forced synchronous `wal.sync()` calls — that bump is a modest, measured
stopgap (chosen because eager allocation makes a larger default cost every
`Engine::open()`, including ~50 test files), not the real fix. Making frame
allocation grow on demand up to `capacity` (rather than pre-allocate it)
would let a much larger ceiling be the default with no tax on small opens —
removing the tradeoff entirely instead of just moving the wall.

0. **Item 18 — Engine access & introspection contract — ✅ SHIPPED 2026-07-13**
   (branch `18-engine-access-contract-impl`). Delivered the `information_schema`-
   style **queryable catalog** (`information_schema.{tables,columns,
   table_constraints,key_column_usage,referential_constraints}` +
   `unidb_catalog.indexes`) as synthesized virtual relations SELECTable over the
   normal query surface — no app REST endpoints — plus the Application Builder's
   Guide (`docs/engine_access_guide.md`) stitching the access/query/type/error
   surface together. Pure read-side projection over metadata that already
   parses+persists (M11); no format bump. Metrics/closeout in `PROGRESS.md`.

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
   ever picked up it takes the next free number (`25_…`).
3. **Parallel-scan follow-ups** (filed in `parallel_scan.md`, lower ROI):
   `SUM`/`AVG`/`GROUP BY` partial aggregate; `LIMIT` early-stop; server
   `ReadHandle` parallelism; a visibility-map fast count. (Default-on + worker
   governance already shipped as #15.)
4. **Item 19 — SQL surface gaps (`19_sql_surface_gaps.md`, NOT STARTED).** The
   tracked list of unsupported query constructs surfaced by Milestone 18's guide:
   `CASE`/`COALESCE` (G1, and the blocker for `FULL OUTER … USING`), `FULL OUTER
   JOIN` (G2), set ops `UNION`/`INTERSECT`/`EXCEPT` (G3), `ORDER BY` on a
   non-projected expr (G4), `RETURNING` (G5), `NATURAL JOIN` (G6, low ROI —
   desugars to the now-supported `USING`), window funcs / recursive CTEs (G7,
   milestone-sized), `SELECT` without `FROM` (G8), **`LIKE`/`ILIKE` pattern
   matching (G9, high ROI — the studio record browser lost contains/starts/ends
   filters to it)**, and **row-path predicate parity for `IS NULL`/`LIKE` so
   filters work off the planner path incl. under `NEAR` (G10)**. Pick individual
   row-path predicate parity for `IS NULL`/`LIKE` (G10), and **full-text search
   has no SQL/REST surface — embed-only `Engine::search_fulltext`, unusable from a
   browser (G11)**. Pick individual gaps as focused improvements; the doc carries
   a per-gap scope/ROI read.
5. **Attach-client session support** (filed in `rest_api_enrichment.md`,
   shipped item 12's one optional follow-up): wrap `X-Txn-Id` sessions +
   `/rows/batch` + cursors in `unidb-attach`.
7. **Storage/recovery follow-ups (filed 2026-07-13 from the guide's limitations
   table; engine-internal, so higher risk than the service lanes — crash
   harness is the hard gate):** **#26 event queue at scale** (sequence index →
   O(new events) polling + push-vs-poll; highest ROI, directly improves item
   20's dispatcher), **#27 vacuum** (per-table accounting + cost throttle +
   whole-table compaction; most self-contained), **#28 replication**
   (time-based PITR + logical replication; milestone-sized). **Parallel note:
   #26 and #28 both edit `lib.rs`+`wal.rs` — do NOT run them concurrently; #27
   (`autovacuum.rs`+`heap.rs`) is safe to run alongside either.**
6. **Supabase-track service milestones (filed 2026-07-13, ordered by
   recommended build sequence — each has its own spec file):**
   **#20 events/realtime dispatcher** (~80% exists in-engine via M4; highest
   demo value; unblocks #23's outbox) → **#21 observability metrics
   enrichment — ✅ SHIPPED 2026-07-13** (branch `21-observability-metrics`;
   lock-free per-chokepoint metrics via `stats()`/`GET /stats` + `/metrics`;
   the horizon-age gauge is the item-16 lesson; widget-traceability table in
   `docs/engine_access_guide.md` §9) → **#22 logs surface** (JSON + correlation
   ids + bounded `/logs`) →
   **#23 storage service — ✅ SHIPPED 2026-07-13** (branch `23-storage-service`,
   PR #64; `unidb-storage` crate — MinIO/S3 over engine metadata + LOB tiering,
   outbox/reconciler, presigned URLs; engine unchanged) → **#24 authz v2**
   (per-op RLS policies + `WITH CHECK` + SQL-native roles; deliberately last —
   deepest semantics).
7. **#25 multi-page catalog** (`25_multipage_catalog.md`, Improvement, NOT
   STARTED) — **surfaced by #23**: the whole catalog (table defs + stats) is one
   ~8 KiB page blob, so a wide schema / accumulated stats overflow with
   `HeapFull`; #23 had to work around it (compact schema, DDL up front). Extends
   item 10 (which moved page-lists out). Recommended first cut: split `stats`
   out of the blob; then evaluate multi-page vs self-hosting catalog.

## How to update this file

- **Start** an item → set status to 🔄 IN PROGRESS; if it's a "Next up"
  candidate, create its `NN_<slug>.md` (next free number) and add a Registry row.
- **Ship** it → status → ✅ SHIPPED with the `PROGRESS.md` entry name.
- Keep this the source of truth for *what exists and where it stands*; keep
  metrics in `PROGRESS.md` and running state in `MEMORY.md`.
