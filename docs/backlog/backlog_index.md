# Backlog index

> The **single at-a-glance registry** of every backlog effort — its number, type,
> and status (pending vs completed) — plus what's planned next. Naming & lifecycle
> rules: [`CONVENTIONS.md`](CONVENTIONS.md). Shipped metrics: `PROGRESS.md`.
>
> **The number is a stable ID** (assigned once, never renumbered — links stay
> valid). **Existing files keep their names**; every **new** backlog file is named
> `NN_<slug>.md` where `NN` is its number here. **Next new file → `104_…`.**
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
| 24 | `24_authz_v2_policies.md` | Milestone | 🔄 PARTIAL — Z1 (DDL) + Z2 (per-op policies) + Z3(JWT) + Z5 (catalog) + Z6 (`current_user` + `/auth/preview`) SHIPPED 2026-07-19. **REMAINING (2026-07-20 fresh-mind review, live-probe confirmed): Z3 write-side `WITH CHECK` on UPDATE — ESCAPE CONFIRMED (`UPDATE … SET user_id='bob'` transfers a row out of the owner's policy; probe archived in scratchpad); bootstrap-mode silent no-enforcement (policies inert until first `CREATE USER` — no warning); Z4 (role inheritance/column grants).** See item 100 for the separate login-endpoint gap. |
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

| 51 | `51_select_join_hash_join.md` | Performance | ✅ SHIPPED Phase B 2026-07-20 (PR #171) — in-memory hash join for equi-joins; `HJ_BUILD_ROWS`/`HJ_PROBE_ROWS` counters; Table 5 SELECT JOIN 0.49× PG. Phase A shipped 2026-07-16. |
| 52 | `52_update_delete_predicate_decode_pushdown.md` | Performance | ✅ SHIPPED 2026-07-16 (correction 2026-07-19: this row previously said NOT STARTED — stale) — `MatchedRows` carries raw bytes; DELETE cols/row 6.00→2.00, dec/row→0.00, +10% throughput. PR #131 MERGED. UPDATE's cols/row=8 remainder is structural (new version needs the full row) and profiling (items 75–84 era) shows decode is not a material UPDATE cost. |
| 53 | `53_fk_update_skip_unchanged_recheck.md` | Improvement | ✅ SHIPPED 2026-07-16 (correction 2026-07-19: this row previously said NOT STARTED — stale, and caused a bad ROI pick) — `has_fk_refs_in_set` gate in `exec_update` skips FK locks+checks when FK col not in SET; 40,423→62,281 rec/s (+54%). See PROGRESS.md "Item 53". |
| 54 | `54_select_filtered_arena_alloc.md` | Performance | ✅ SHIPPED 2026-07-16 — Phase A: `scan_page_visit` + `project_row_drain` + `parallel_resolve_partitions`. SELECT filtered 0.50×→0.57× PG at 100k rows (+24%); RSS 315→296 MiB. PR #135. See PROGRESS.md. |
| 55 | `55_event_queue_small_table_overhead.md` | Improvement | ✅ RESOLVED 2026-07-19 — the W4/W0 anomaly was a bench artefact (WAL measurement window not normalized); fixed by `sync_wal()`+`checkpoint()` before the window + structural WAL-byte gate (commit 92e8713). No engine change needed. |
| 56 | `56_crud_gap_write_batching_parallel_agg.md` | Performance | ✅ SHIPPED — Step 1 (parallel GROUP BY 1.14× PG) 2026-07-16; Steps 2+3 (WAL_XMAX_BATCH DELETE) 2026-07-17 PR #137; Step 4 (logical B-tree INSERT WAL 8837→655 B/row, +25% rec/s) 2026-07-17 PR #139 |
| 57 | `57_next_perf_improvements.md` | Performance | ⏳ NOT STARTED — D4 HOT sign-off analysis (defer: ceiling 0.08×); parallel DELETE scan (0.07→0.15–0.20×, HIGH ROI); W4/W0 overhead root-cause; ROI ranking: #1 parallel DELETE, #2 HOT. Fable-5 arch review 2026-07-17. |
| 58 | `58_hot_update.md` | Performance | ✅ SHIPPED 2026-07-17 — HOT-equivalent UPDATE: same-page insert when no indexed col in SET; FSM pre-screen fast-path for full pages; vacuum B-tree patch for HOT chain heads; FORMAT_VERSION 7→8; P59a/P59b crash tests. Measured 0.043× PG at 100k packed rows (HOT fires only when pages have slack; no regression). PR #141 MERGED. See PROGRESS.md. |
| 59 | `59_select_filtered_optimisations.md` | Performance | ✅ SHIPPED 2026-07-17 — Fix 1: `COLS_DECODED` gated behind `DIAGNOSTICS_ENABLED`; Fix 2: `Expr::ColumnSlot` pre-binding eliminates per-row linear String scan; Fix 3: `RawFilter` / `try_raw_i64_at` late materialisation skips `deform_row` on rejected rows at 5% selectivity. 3 new tests; 415 unit + 46 crash harness PASS. PR #142 MERGED. |
| 60 | `60_event_queue_serde_json.md` | Performance | ✅ SHIPPED 2026-07-17 — replaced `serde_json::json!` + `row_to_json` (Value AST heap allocation) in `send_event_capture` with manual string builder (`build_event_envelope_str` + `write_row_json`); VECTOR(128) no longer boxes 128 `JsonValue::Number`s. W4/W0 at 100k: 1.70× → 1.49× (gate ≤1.50× MET). See PROGRESS.md. |
| 61 | `61_replaced_stack_bench.md` | Performance | ✅ SHIPPED 2026-07-17 — true replaced-stack benchmark: Postgres (row + pgvector + graph adjacency, 3 separate autocommit connections) + Redpanda (separate Docker container, real inter-process TCP, Kafka protocol via rskafka). Table 4.1 gated on `MM_REPLACED_STACK_REALISTIC=1`. PR #144 MERGED. |

| 63 | `63_disk_hnsw_planning.md` | Performance | ✅ SHIPPED 2026-07-17 — on-disk HNSW replaces IVF-Flat. recall@10=0.964 at 1k×dim128 (≥0.95 gate PASS). src/hnsw_index.rs; 48/48 crash tests (P60a+P60b); 669 tests; clippy/fmt clean. PR pending. |
| 62 | `62_ivf_scale_validation.md` | Performance | ✅ SHIPPED 2026-07-17 — bench: IVF recall@10/latency at 1k/10k/100k; recall=0.421 at 100k unlocks item 63 gate. PR #145 MERGED. |
| 64 | `64_delete_lazy_xmax.md` | Performance | 🔄 INVESTIGATION COMPLETE — two bottlenecks profiled: (1) CRC-per-mutation in `set_xmax` (807 ns/row, 87.5% at 25k scale); (2) `latch_fetch` blowup 1.2→611 µs/page at 100k (mmap/OS cold-page). Lazy xmax ruled infeasible (MVCC violation). Fix A (remove `write_crc()` from `set_xmax`) **SHIPPED** (correction 2026-07-19: "ready to implement" was stale — the skip + doc comment are in `page.rs::set_xmax`; DELETE 0.04→0.06× recorded in PROGRESS). Fix B's latch+fetch blowup root cause identified by the 2026-07-19 profiling review (clock-sweep evictions + per-fetch CRC verify) — addressed by item 78 (shipped) and item 86 (filed). Generalization of Fix A to all mutations/fetches = **item 86**. |
| 65 | `65_hnsw_insert_node_cache.md` | Performance | ✅ SHIPPED 2026-07-18 — per-insert `NodeCache` eliminates repeated DiskBTree lookups during HNSW beam search (~3200 → ~200 unique node fetches per insert). See PROGRESS.md "Item 65". |
| 66 | `66_parallel_delete_scan.md` | Performance | ✅ SHIPPED 2026-07-18 — `parallel_collect_matching` in `parallel_scan.rs`; A3-gate-aware `'collect` block in `exec_delete`; sort before `delete_many`; 48/48 crash PASS; `parallel_delete_matches_serial` PASS. Docker bench pending. See PROGRESS.md "Item 66". |
| 67 | `67_async_hnsw_index_build.md` | Performance | ✅ SHIPPED 2026-07-20 (PR #171) — async HNSW background worker (`HnswWorker` thread + `ExecCtx.hnsw_tx`); HNSW graph-stitching decoupled from commit critical path. See PROGRESS.md "Items 67/51/68/69". |
| 68 | `68_hint_bits.md` | Performance | ✅ SHIPPED 2026-07-20 (PR #171) — lazy txn-state cache in tuple header flags (`HINT_XMIN_COMMITTED`/`HINT_XMAX_ABORTED`); SELECT filtered 0.55→0.74× PG (+35%). See PROGRESS.md "Items 67/51/68/69". |
| 69 | `69_fill_factor.md` | Performance | ✅ SHIPPED 2026-07-20 (PR #171) — fill-factor page reservation (default 80%); FSM 8-level granularity; UPDATE HOT 1.12→1.51× PG (+35%). See PROGRESS.md "Items 67/51/68/69". |
| 70 | `70_seq_scan_prefetch.md` | Performance | ⏳ NOT STARTED — `madvise(MADV_WILLNEED)` read-ahead hint during seqscan (N pages ahead of cursor); cold-cache seqscan latency improvement; no-op on unsupported platforms. |
| 71 | `71_cross_page_hot.md` | Performance | ✅ SHIPPED 2026-07-18 — cross-page HOT chains; `HOT_NEXT_XPAGE=0xFFFE`; `WAL_HOT_XPAGE_HEAD` type 17; FORMAT_VERSION 8→9; B-tree not updated on full-page UPDATE; P_xhot_a + P_xhot_b crash tests; 50/50 crash + 431 unit PASS. See PROGRESS.md "Item 71". |
| 72 | `72_hnsw_query_latency.md` | Performance | ✅ SHIPPED 2026-07-19 — `HnswL0Cache` L0 neighbour list cache (cd94d71) + item 73 vector hot cache together achieve warm ≤5 ms at 10k (2.38 ms measured, 11.2× speedup). See PROGRESS.md. |
| 73 | `73_hnsw_vector_hot_cache.md` | Performance | ✅ SHIPPED 2026-07-19 — `HnswVecCache` (encoded_rid → Vec<f32>); snapshot-then-merge in `exec_select_near`; 10k warm 2.38 ms / 18.7× speedup at 1k; Docker bench pending. See PROGRESS.md. |
| 74 | `74_hot_update_batch.md` | Performance | ✅ SHIPPED — commit 4dd81ac (hot_update_many Phase B+A) is below 7a25a5e; the items 75–84 Docker bench (report_20260718_232622.md) covers this binary: UPDATE HOT 453k rec/s / **0.62×** vs PG. No separate run needed — that IS item 74's bench. See PROGRESS.md items 75–84. |
| 75–84 | (no separate files) | Performance | ✅ SHIPPED 2026-07-19 — DELETE + UPDATE perf sprint (PR #150). Items tracked as a bundle in PROGRESS.md "Items 75–84". |
| 85 | `85_concurrency_hang_cross_row_churn.md` | Improvement | ✅ SHIPPED 2026-07-19 — production-default concurrency hang (cross-row UPDATE churn, toggle=on, no index); root cause: Phase B→A ordering in hot_update_many left orphaned tuples on WriteConflict; fix: swap to A→B→C. See PROGRESS.md "Item 85". |
| 86 | `86_crc_storage_boundary.md` | Performance | ✅ SHIPPED 2026-07-19 (PR #155) — CRC at storage boundary: alloc-free `compute_crc` + pool-hit skip; `insert_versioned` no longer clones 8 KiB. UPDATE HOT +55% native (45k→70k rec/s). See PROGRESS.md "Wave 1". |
| 87 | `87_fill_page_cursor.md` | Performance | ✅ SHIPPED 2026-07-19 (PR #155) — statement-scoped fill-page cursor in `hot_update_many` / `update_many` Phase B; FSM mutex skipped when cursor has slack. UPDATE HOT +15.6% native (70k→81k rec/s). See PROGRESS.md "Wave 1". |
| 88 | `88_bulk_lock_elision.md` | Performance | ✅ SHIPPED 2026-07-19 (PR #155) — bulk DML skips per-row `LockTable::lock_write`; `release_all` O(phantoms); scenario-10 20/20 PASS. See PROGRESS.md "Wave 1". |
| 89 | `89_wal_background_sealer.md` | Performance | ✅ SHIPPED 2026-07-19 (PR #155) — WAL seal `fsync` moved off append critical path to background thread; p99 flattening. See PROGRESS.md "Wave 1". |
| 90 | `90_btree_batch_maintenance.md` | Performance | ✅ SHIPPED 2026-07-19 (PR #155) — batched B-tree maintenance: collect+sort per-leaf-group, single `latch_exclusive`; UPDATE non-HOT WAL 202→~130 B/row. See PROGRESS.md "Wave 1". |
| 91 | `91_m4_event_source_decision.md` | Improvement | ✅ SHIPPED 2026-07-19 (PR #153) — Option A (executor-capture) approved; WAL-derived stream = `RecordKind::Event` rows in WAL, not physical redo record derivation. See PROGRESS.md "Item 91". |
| 92 | `92_vector_query_next_tier.md` | Performance | ✅ SHIPPED 2026-07-19 (PR #154) — zero-copy dist on vec-cache hits + SIMD f32 accumulation + CREATE INDEX `prefetch_caches`; cold 24ms→1.27ms (19×), disk fetches/q 48→0. See PROGRESS.md "Item 92". |
| 93 | `93_hnsw_arena_layout.md` | Performance | 🔄 IN PROGRESS 2026-07-20 — `L0Arena` flat slab (`arena_data: Vec<i64>` + `arena_offsets: Vec<u32>`) replaces `HashMap<i64,Vec<RowId>>`; stack-buffer hot path in `search_layer_with_vec` eliminates heap alloc per hop; 0 disk fetches on warm path, recall@10 1.000; Docker bench pending (target ≤600 µs at 10k). Branch `perf/item-93-hnsw-arena`. |
| 94 | `94_near_read_only_fast_path.md` | Performance | ⏳ NOT STARTED — `read_snapshot_lightweight()` skips active-snapshot registration + WAL tail pin for standalone read-only NEAR outside user BEGIN. |
| 95 | `95_graph_adjacency_cache.md` | Performance | ✅ SHIPPED 2026-07-20 (PR pending) — lazy warm cache (`AdjacencyCache` DashMap, EdgeRef, approx-LRU eviction); invalidate O(1) on INSERT/DELETE; 8W+8R 100k concurrent test PASS. See PROGRESS.md "Item 95". |
| 96 | `96_query_plan_cache.md` | Performance | ✅ SHIPPED 2026-07-19 (PR #156) — 1,024-entry LRU plan cache keyed by `(sql_hash, schema_epoch)`; schema_epoch bumped on DDL; 537–891× speedup on repeated same-SQL; SELECT filtered 0.44→0.55×. See PROGRESS.md "Item 96". |
| 97 | `97_count_star_statistics.md` | Performance | ✅ SHIPPED 2026-07-19 (PR #161) — `row_count: i64` in `TableDef`; maintained on INSERT/DELETE/TRUNCATE; O(1) COUNT(*) fast path; FORMAT_VERSION 10→11; SELECT COUNT(*) 6.93× vs PG. See PROGRESS.md "Item 97". |
| 98 | `98_sql_insert_throughput.md` | Performance | ✅ SHIPPED 2026-07-19 (PR #157+#159) — `InsertAccum` streaming accumulation: one WAL_BEGIN+COMMIT per VALUES statement; UNIQUE enforcement preserved (per-row index before next row validate). See PROGRESS.md "Item 98". |
| 99 | `99_batch_sql_endpoint.md` | Performance | ✅ SHIPPED 2026-07-19 (PR #162) — `POST /batch-sql`; up to 256 stmts per request; stop_on_error flag; per-statement auth; 400 BATCH_TOO_LARGE. Projects compare.py 109ms→~16ms (15.7×→~2.3×). See PROGRESS.md "Item 99". |
| 100 | `100_dev_login_whoami.md` | Improvement | ✅ SHIPPED 2026-07-20 (PR #168) — GET /auth/meta + POST /auth/login (UNIDB_DEV_LOGIN=1) + GET /auth/whoami; R-a UPDATE WITH CHECK + R-b enforced column also in same PR. See PROGRESS.md "Item 100". |
| 101 | `101_group_commit.md` | Performance | ✅ SHIPPED 2026-07-20 (PR #170) — group-commit dwell window in WAL `sync_up_to`; `PUT /config/group_commit_window_us`; `Engine::wal_fsyncs_count()`. |
| 102 | `102_index_only_scan.md` | Performance | 🔄 Phase A SHIPPED 2026-07-20 (PR #169) — skip `deform_row` for key-col projection; `IDX_ONLY_ROWS` counter; heap.get() still needed for MVCC visibility. Phase B (covering index) pending. |
| 103 | `103_authz_v2_studio_integration_gaps.md` | Improvement | ⏳ NOT STARTED — superuser/no-`sub` callers silently return empty rows when `current_user` RLS policies exist (bypass never fires); stale `CREATE ROLE … SUPERUSER` doc example; missing `role_members`/`users` in catalog paragraph. Re-confirmed live post PR #168. |

| 103 | `103_authz_v2_studio_integration_gaps.md` | Improvement | ✅ SHIPPED 2026-07-20 — superuser/no-sub RLS bypass on concurrent-read path; `ReadHandle::execute_sql_as`; doc fixes (`CREATE ROLE SUPERUSER` → `CREATE USER SUPERUSER`, add `role_members`/`users` to catalog virtual relations). See PROGRESS.md "Item 103". |

Meta docs (not numbered work items): `roadmap.md` (the numbered-phase plan),
`CONVENTIONS.md` (this standard), `engine_internals_doc_prompt.md` (tooling).
**Next new file → `104_…`.**

## Next up — priority order (2026-07-20, post PR #171 merge)

State after 2026-07-20 (items 51/67/68/69/101/102-A all shipped):

| Operation | unidb ÷ PG | Status |
|---|---|---|
| SELECT COUNT(*) | **6.93×** | ✅ well above 1× |
| DELETE all | **7.06×** | ✅ well above 1× |
| DELETE selected | **2.73×** | ✅ well above 1× |
| SELECT GROUP BY | **1.30×** | ✅ above 1× |
| UPDATE HOT | **1.51×** | ✅ well above 1× |
| UPDATE non-HOT | 0.81× | 📈 ceiling ~0.85–0.90× — item 102-B covering index next |
| SELECT filtered | 0.74× | 📈 ceiling ~0.80–0.85× — item 102-B covering index next |
| INSERT per-row | 0.53× | 🔒 structural single-fsync floor; item 101 targets concurrent path |

**Next priority items:**

1. **#102-B Covering index** — `CREATE INDEX … INCLUDE (cols)`; FORMAT_VERSION bump; now main is clean.
2. **#103 Catalog sync dedup** — remove `wal.sync_up_to(catalog_lsn)` after `catalog.persist_only()`; eliminates double-fsync-per-INSERT; catalog row-count recomputed from heap on crash.
3. **#93 HNSW arena layout** — flat `Vec<RowId>` slab; expected W2 latency reduction.
4. **#94 NEAR read-only fast path** — skip full HNSW beam search for exact-match NEAR queries.
5. ~~**#95 graph adjacency cache**~~ — ✅ SHIPPED 2026-07-20.

**What is NOT in this list:**
- Parallel DML apply: held in reserve; not justified at current acceptance band.
- AuthZ v2 (item 24): fully shipped (R-a/R-b + item 100, PR #168 2026-07-20).

## How to update this file

- **Start** an item → set status to 🔄 IN PROGRESS; if it's a "Next up"
  candidate, create its `NN_<slug>.md` (next free number) and add a Registry row.
- **Ship** it → status → ✅ SHIPPED with the `PROGRESS.md` entry name.
- Keep this the source of truth for *what exists and where it stands*; keep
  metrics in `PROGRESS.md` and running state in `MEMORY.md`.
