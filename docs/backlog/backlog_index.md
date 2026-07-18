# Backlog index

> The **single at-a-glance registry** of every backlog effort — its number, type,
> and status (pending vs completed) — plus what's planned next. Naming & lifecycle
> rules: [`CONVENTIONS.md`](CONVENTIONS.md). Shipped metrics: `PROGRESS.md`.
>
> **The number is a stable ID** (assigned once, never renumbered — links stay
> valid). **Existing files keep their names**; every **new** backlog file is named
> `NN_<slug>.md` where `NN` is its number here. **Next new file → `58_…`.**
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
| 37 | `37_lazy_buffer_pool_growth.md` | Improvement | ✅ SHIPPED 2026-07-16 — `BufferPool::open` now pre-allocates only 256 frames (`INITIAL_SLAB_FRAMES`); `find_victim` grows by one frame on demand up to `capacity`; `DEFAULT_POOL_CAPACITY` raised 65536 → 2,000,000. `Engine::open()` cost stays ~256-frame (~6 KiB) regardless of ceiling. See PROGRESS.md |
| 38 | `38_param_type_coercion.md` | Improvement | ⏳ NOT STARTED — engine rejects `WHERE int_col = $1` when `$1` is bound as `Text("20")`; should coerce losslessly to the column type (standard SQL behaviour; PostgreSQL/SQLite/MySQL all do this). Studio workaround (`bindForColumn` in RecordBrowser) applied 2026-07-15 but does not cover other clients or expression contexts. |
| 39 | `39_pk_fk_relational_stress_bench.md` | Performance | ✅ SHIPPED — new Table 5 in `scripts/report.sh`'s multi-model report: a real `customers`/`orders` PK/FK schema (previously the whole report had zero `FOREIGN KEY` usage anywhere), throughput vs Postgres, plus pass/fail correctness proofs that both engines reject an invalid FK and RESTRICT a still-referenced DELETE. Made fair by item 36 (FK row-level enforcement, shipped the same day). See PROGRESS.md |

| 40 | `40_btree_bulk_build.md` | Performance | ✅ SHIPPED 2026-07-15 — sort-then-bulk-load CREATE INDEX backfill: collect (key, row_id) pairs, sort, `insert_many` (one WAL mini-txn / one fsync). 134.2 s → 12.0 s (**11.2×**) on 540k rows. P40 crash test added (38/38). See PROGRESS.md. |
| 41 | `41_near_vec_distance.md` | Improvement | ✅ SHIPPED 2026-07-14 — `exec_select_near` threads its already-computed re-rank distance through to projection as a virtual `vec_distance` column (`Literal::Float`, ascending); no catalog/format change. See PROGRESS.md. |
| 42 | `42_bench_harness_buffer_pool.md` | Improvement | ✅ SHIPPED — `benches/decompose.rs` never sized its buffer pool, so any report sweeping into 1M+ rows silently hit `BufferPoolFull` and understated unidb's real throughput (measured 1,228 rec/s vs the true 15,905 rec/s at 1M rows, ~13× recovered). New `bench_engine_open()` helper opens every bench engine with a 2,000,000-frame pool. See PROGRESS.md |
| 43 | `43_a3_gate_size_aware_selectivity.md` | Improvement | ✅ SHIPPED 2026-07-15 — size-aware cost model (`page_count > BTREE_STARTUP + matched×HEAP_FETCH_SEQ_EQUIV`), best-arm predicate selection (`find_best_indexable_btree_predicate` prefers `k<N` over `k>=0`), and A3 gate added to exec_select. Crossover at ~2600 rows for 50% selectivity; 3 permanent regression tests. PR pending. |
| 44 | `44_bulk_delete_batched_wal.md` | Performance | ✅ SHIPPED 2026-07-16 (PR #119) — `Heap::delete_many` groups already page-sorted row_ids by page_id, one WAL mini-txn per page instead of per row. WAL B/row 230 → 107 (−53%), 416k rec/s at 5000 rows. See PROGRESS.md "Items 47 + 44". |
| 45 | `45_select_filtered_parallel_btree_scan.md` | Performance | ✅ SHIPPED 2026-07-16 — Lever 1 (search_range_partition, PR #125) + Lever 2 (pre-spawned worker pool, PR #123). Lever 3 (arena alloc) deferred. |
| 46 | `46_select_grouped_hash_aggregate.md` | Performance | ✅ SHIPPED 2026-07-15 (PR #117) — B2 decode-pushdown extended into aggregate path (column mask to GROUP-BY exprs only); integer-keyed HashMap specialisation; DELETE-all small-candidate guard. See PROGRESS.md. |
| 47 | `47_update_delete_write_throughput.md` | Performance | ✅ SHIPPED (Phase A) 2026-07-16 (PR #119) — B-tree in-place RowId patch for unchanged-key UPDATE (`patch_many` batched across secondary + unique-enforcement indexes). WAL B/row 619 → 465 (−25% at 500-row scale). Phase B (vectorised predicate scan) and Phase C (HOT-equivalent chain) remain open follow-ons. See PROGRESS.md "Items 47 + 44". |
| 48 | `48_delete_all_truncate_fast_path.md` | Performance | ✅ SHIPPED 2026-07-15 (PR #117) — `TRUNCATE TABLE t` SQL surface + `Heap::truncate()` (single WAL record + heap/index reset); opportunistic DELETE-all → truncate routing when no FK children and no CDC subscribers. See PROGRESS.md. |
| 49 | `49_bench_pg_connect_no_timeout_hang.md` | Improvement | ✅ SHIPPED 2026-07-16 — `benches/decompose.rs` opened every Postgres connection with no `connect_timeout`; an unreachable/unresponsive `PG_URL` (wrong host, firewalled, container still starting) blocked on the OS TCP SYN-retry ceiling (~2 min/attempt, confirmed empirically) across 24 call sites with zero output — the real cause of `scripts/report.sh` reports "hanging indefinitely". New `pg_dial()` helper sets `connect_timeout` (default 10s, `PG_CONNECT_TIMEOUT_SECS`); all call sites route through it. Verified: unreachable PG_URL now fails the whole report in 14.6s instead of hanging. See PROGRESS.md. |
| 50 | `50_patch_many_infinite_loop.md` | Improvement | ✅ SHIPPED 2026-07-16 — **critical**: `DiskBTree::patch_many` (item 47) genuinely infinite-loops, single-threaded, 100% CPU, on an unchanged-key `UPDATE` whenever the very first patch in a leaf-group has a key outside that leaf's *current* `entries.first()/last()` (plausible after any split) — the bounds check gated the first entry too, so the loop index never advanced. Confirmed live via `gdb -p <pid> -batch -ex bt` (identical stack twice). This is why it was never caught: Table 3 (the only report section touching this path) only runs when Postgres is reachable, and this session's item 49 fix was the first time that condition was met. Fixed: bounds check now only gates *additional* (`j > i`) batching, never `j == i`. New permanent regression test confirmed to catch the bug pre-fix (30s hang deadline) and pass post-fix (~1s). See PROGRESS.md. |

| 51 | `51_select_join_hash_join.md` | Performance | ⏳ PHASE A DONE 2026-07-16 — predicate pushdown into base scans + integer key hash fast path + INLJ-via-unique_index_root revert; 0.31→0.59× PG. Phase B (≥0.70×) pending. See PROGRESS.md. |
| 52 | `52_update_delete_predicate_decode_pushdown.md` | Performance | ⏳ NOT STARTED — item 47 Phase B: cols/row=8 on UPDATE, cols/row=6 on DELETE proves full decode on predicate-scan path. Extend B2 deform_row mask to matching_rows write path. DELETE: 0.16→0.30-0.40×; UPDATE: 0.14→0.18-0.22×. |
| 53 | `53_fk_update_skip_unchanged_recheck.md` | Improvement | ⏳ NOT STARTED — FK UPDATE 0.06× PG (17× behind); executor re-checks FK constraint on every UPDATE row unconditionally even when FK column is not in SET clause. Skip when FK col not in SET. 0.06→0.12-0.18×. |
| 54 | `54_select_filtered_arena_alloc.md` | Performance | ✅ SHIPPED 2026-07-16 — Phase A: `scan_page_visit` + `project_row_drain` + `parallel_resolve_partitions`. SELECT filtered 0.50×→0.57× PG at 100k rows (+24%); RSS 315→296 MiB. PR #135. See PROGRESS.md. |
| 55 | `55_event_queue_small_table_overhead.md` | Improvement | ⏳ NOT STARTED — W4/W0=3.93× at 1k rows (Δ event=1.29ms vs 0.12ms at 10k); 10× anomaly unexplained; investigate before optimising (vacuum threshold, sequence index, WAL group-commit). |
| 56 | `56_crud_gap_write_batching_parallel_agg.md` | Performance | ✅ SHIPPED — Step 1 (parallel GROUP BY 1.14× PG) 2026-07-16; Steps 2+3 (WAL_XMAX_BATCH DELETE) 2026-07-17 PR #137; Step 4 (logical B-tree INSERT WAL 8837→655 B/row, +25% rec/s) 2026-07-17 PR #139 |
| 57 | `57_next_perf_improvements.md` | Performance | ⏳ NOT STARTED — D4 HOT sign-off analysis (defer: ceiling 0.08×); parallel DELETE scan (0.07→0.15–0.20×, HIGH ROI); W4/W0 overhead root-cause; ROI ranking: #1 parallel DELETE, #2 HOT. Fable-5 arch review 2026-07-17. |
| 58 | `58_hot_update.md` | Performance | ✅ SHIPPED 2026-07-17 — HOT-equivalent UPDATE: same-page insert when no indexed col in SET; FSM pre-screen fast-path for full pages; vacuum B-tree patch for HOT chain heads; FORMAT_VERSION 7→8; P59a/P59b crash tests. Measured 0.043× PG at 100k packed rows (HOT fires only when pages have slack; no regression). PR #141 MERGED. See PROGRESS.md. |
| 59 | `59_select_filtered_optimisations.md` | Performance | ✅ SHIPPED 2026-07-17 — Fix 1: `COLS_DECODED` gated behind `DIAGNOSTICS_ENABLED`; Fix 2: `Expr::ColumnSlot` pre-binding eliminates per-row linear String scan; Fix 3: `RawFilter` / `try_raw_i64_at` late materialisation skips `deform_row` on rejected rows at 5% selectivity. 3 new tests; 415 unit + 46 crash harness PASS. PR #142 MERGED. |
| 60 | `60_event_queue_serde_json.md` | Performance | ✅ SHIPPED 2026-07-17 — replaced `serde_json::json!` + `row_to_json` (Value AST heap allocation) in `send_event_capture` with manual string builder (`build_event_envelope_str` + `write_row_json`); VECTOR(128) no longer boxes 128 `JsonValue::Number`s. W4/W0 at 100k: 1.70× → 1.49× (gate ≤1.50× MET). See PROGRESS.md. |
| 61 | `61_replaced_stack_bench.md` | Performance | ✅ SHIPPED 2026-07-17 — true replaced-stack benchmark: Postgres (row + pgvector + graph adjacency, 3 separate autocommit connections) + Redpanda (separate Docker container, real inter-process TCP, Kafka protocol via rskafka). Table 4.1 gated on `MM_REPLACED_STACK_REALISTIC=1`. PR #144 MERGED. |

| 63 | `63_disk_hnsw_planning.md` | Performance | ✅ SHIPPED 2026-07-17 — on-disk HNSW replaces IVF-Flat. recall@10=0.964 at 1k×dim128 (≥0.95 gate PASS). src/hnsw_index.rs; 48/48 crash tests (P60a+P60b); 669 tests; clippy/fmt clean. PR pending. |
| 62 | `62_ivf_scale_validation.md` | Performance | ✅ SHIPPED 2026-07-17 — bench: IVF recall@10/latency at 1k/10k/100k; recall=0.421 at 100k unlocks item 63 gate. PR #145 MERGED. |
| 64 | `64_delete_lazy_xmax.md` | Performance | 🔄 INVESTIGATION COMPLETE — two bottlenecks profiled: (1) CRC-per-mutation in `set_xmax` (807 ns/row, 87.5% at 25k scale); (2) `latch_fetch` blowup 1.2→611 µs/page at 100k (mmap/OS cold-page). Lazy xmax ruled infeasible (MVCC violation). Fix A (remove `write_crc()` from `set_xmax`) ready to implement. Fix B (latch+fetch root cause) needs diagnostic first. |
| 65 | `65_hnsw_insert_node_cache.md` | Performance | ✅ SHIPPED 2026-07-18 — per-insert `NodeCache` eliminates repeated DiskBTree lookups during HNSW beam search (~3200 → ~200 unique node fetches per insert). See PROGRESS.md "Item 65". |
| 66 | `66_parallel_delete_scan.md` | Performance | ✅ SHIPPED 2026-07-18 — `parallel_collect_matching` in `parallel_scan.rs`; A3-gate-aware `'collect` block in `exec_delete`; sort before `delete_many`; 48/48 crash PASS; `parallel_delete_matches_serial` PASS. Docker bench pending. See PROGRESS.md "Item 66". |
| 67 | `67_async_hnsw_index_build.md` | Performance | 📋 PLANNED 2026-07-18 — async HNSW: decouple index build from commit critical path (W4/W0 → ~1.1×). ef_construction reduction ruled out (recall@10=0.937 at ef=100,10k — fails gate). See PROGRESS.md "Item 67 planning". |
| 68 | `68_hint_bits.md` | Performance | ⏳ NOT STARTED — lazy hint bits in tuple header to short-circuit `txn_state(xmin/xmax)` lookup on committed tuples; ~5–10% SELECT gain; no WAL write; no FORMAT_VERSION bump if reserved bytes available. |
| 69 | `69_fill_factor.md` | Performance | ⏳ NOT STARTED — `CREATE TABLE … WITH (fill_factor=70)` reserves page slack for same-page HOT (item 58); INSERT stops at configured threshold; UPDATE-heavy tables avoid cross-page chains. |
| 70 | `70_seq_scan_prefetch.md` | Performance | ⏳ NOT STARTED — `madvise(MADV_WILLNEED)` read-ahead hint during seqscan (N pages ahead of cursor); cold-cache seqscan latency improvement; no-op on unsupported platforms. |
| 71 | `71_cross_page_hot.md` | Performance | ✅ SHIPPED 2026-07-18 — cross-page HOT chains; `HOT_NEXT_XPAGE=0xFFFE`; `WAL_HOT_XPAGE_HEAD` type 17; FORMAT_VERSION 8→9; B-tree not updated on full-page UPDATE; P_xhot_a + P_xhot_b crash tests; 50/50 crash + 431 unit PASS. See PROGRESS.md "Item 71". |
| 72 | `72_hnsw_query_latency.md` | Performance | ⏳ NOT STARTED — in-memory L0 neighbour-list cache to collapse HNSW query latency from 25 ms → ≤5 ms at 10k vectors (ffsdb gap: 223×); lazy per-query warm-up; generation-based invalidation on insert; 256 MiB gate. |
| 73 | `73_hnsw_vector_hot_cache.md` | Performance | ⏳ NOT STARTED (placeholder) — process-lifetime vector hot cache (node_id → Vec<f32>) eliminating ~100 KB random reads per NEAR query; follow-up to item 72. |
| 74 | `74_hot_update_batch.md` | Performance | 🔄 IN PROGRESS — batch mini-txn for HOT UPDATE; `Heap::hot_update_many` Phase B+A; reduces 150k mutex/Vec/CRC32 passes to ~2k for 50k rows; P74 crash test; 4 perf_item74 tests; Docker bench pending. |

Meta docs (not numbered work items): `roadmap.md` (the numbered-phase plan),
`CONVENTIONS.md` (this standard), `engine_internals_doc_prompt.md` (tooling).
**Next new file → `75_…`.**

## Next up — priority order (2026-07-16, calibrated on `052432` Docker baseline)

Ordered by measured ROI. Each item has its own spec file (see Registry above). Reorder as new data arrives.

**#51 Phase B — SELECT JOIN ≥0.70× PG (`51_select_join_hash_join.md`) — Phase A done (0.59×).**
Remaining gap is row-decode cost: late-materialization (only decode referenced columns) and
scan-side decode reuse. Candidates: (a) `deform_row` mask in the join executor scan, (b)
columnar projection pushdown. Not yet filed as a separate item — consider merging into #52 scope
since the fix is the same decode-pushdown mechanism.

**#52 — UPDATE/DELETE predicate decode pushdown (`52_update_delete_predicate_decode_pushdown.md`).**
cols/row=8 (UPDATE) and cols/row=6 (DELETE) measured in `030325`: we decode all columns on the
predicate-scan path when we only need the WHERE column(s). Extends B2 (`deform_row` mask) already
shipped for SELECT. DELETE gets the larger gain (no write-step decode needed). UPDATE gain bounded by
insert-new-version MVCC write cost. Can develop alongside #51 (different executor sections).

**#53 — FK UPDATE: skip re-check when FK col not in SET (`53_fk_update_skip_unchanged_recheck.md`).**
0.06× PG = 17× behind. Trivial executor fix: skip `enforce_fk_child_insert_update` when the FK
column is not in the SET clause. Independent of #51 and #52; can run in a parallel worktree.

**#54 — SELECT filtered arena allocation (`54_select_filtered_arena_alloc.md`).**
0.42× PG. Phase B decode already applied (cols/row=4.00). Per-row Vec<Literal>+String allocation
is the remaining addressable cost. Residual gap beyond ~0.55× is Postgres's parallel worker count
at 18 cores — architectural. Independent of all other items.

**#55 — Event-queue 1k-row investigation (`55_event_queue_small_table_overhead.md`).**
W4/W0=3.93× at 1k rows (Δ event=+1.29ms vs +0.12ms at 10k). Investigation-first item: profile
before optimising. Does not block #51–54. Run in parallel with any of the above.

**Parallel note:** #51 (executor join), #52 (matching_rows decode), #53 (FK check), #54 (arena
alloc), #55 (event investigation) touch distinct code paths and can all run in separate worktrees
simultaneously with no file-level conflicts. Recommended: start #51 + #53 in one session, #52 in
a second, #54 in a third, #55 (investigation) in a fourth.

**What is NOT in this list:**
- Item 47 Phase C (HOT-equivalent chain): requires FORMAT_VERSION bump (locked decision D4).
  Measure Phase B results first; file Phase C separately only if the gap after Phase B justifies
  reopening a locked decision.
- Per-row INSERT gap (0.24× PG): WAL FPI overhead, structural. Per §1, expected to lose this.
- SQL surface gaps (item 19): non-performance; tracked separately.

## How to update this file

- **Start** an item → set status to 🔄 IN PROGRESS; if it's a "Next up"
  candidate, create its `NN_<slug>.md` (next free number) and add a Registry row.
- **Ship** it → status → ✅ SHIPPED with the `PROGRESS.md` entry name.
- Keep this the source of truth for *what exists and where it stands*; keep
  metrics in `PROGRESS.md` and running state in `MEMORY.md`.
