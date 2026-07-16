# MEMORY.md

> **Read this FIRST every session. Update it LAST every session.**
> This is the running state of the implementation — what exists, what's in
> progress, what's next. Rules & locked decisions live in `CLAUDE.md`.
> Shipped-milestone records + metrics live in `PROGRESS.md`.
>
> When you update this file, stamp the log with the **actual current system
> date** — never copy a date from above.

---

## Current status

- **Bench harness Postgres connect-timeout fix (item 49) — SHIPPED 2026-07-16,
  branch `49-pg-connect-timeout`.**
  Investigated a report that `scripts/report.sh` "runs in indefinite mode."
  Root cause: `benches/decompose.rs` opened every Postgres connection via
  `Client::connect(url, NoTls)` with no `connect_timeout` — an unreachable
  `PG_URL` (wrong host, firewalled, container still starting) blocks on the
  OS TCP SYN-retry ceiling (confirmed empirically: ~2 min/attempt) across 24
  call sites, with zero output. Ruled out as causes (audited, no bug found):
  item 47/44's new per-page latching (single latch at a time, consistent
  ordering), `lock_mgr.try_acquire_write` (non-blocking `WaitPolicy::NoWait`),
  the parallel-scan worker governor item 15 (non-blocking admission, degrades
  to serial), `conc_matrix`'s deadlock handling (already bounded to 120s/cell,
  isolated per-cell tempdir engine). Fix: new `pg_dial()` helper sets
  `connect_timeout` (default 10s, `PG_CONNECT_TIMEOUT_SECS`); all 24 call
  sites route through it. Verified: unreachable `PG_URL` now fails the whole
  report in 14.6s (was: indefinite hang) — reachable-Postgres runs unaffected
  (numbers identical, timeout never fires). `cargo build`/`clippy -D warnings`
  clean, bench-harness-only change (no engine/format/WAL touch). Full
  1k/10k-row report re-generated with real Postgres columns on this branch to
  hand off for the next optimization decision.
- **UPDATE in-place B-tree patch + DELETE batched mini-txn (items 47 + 44) —
  SHIPPED 2026-07-16, branch `47-44-perf-batch`, PR #119 (MERGED to main).**
  Item 47: `init_patch_batches` now batches unique-enforcement index patches
  alongside secondary BTree patches; `flush_patch_batches` calls `patch_many`
  once per batch after the row loop. WAL B/row 619 → 465 (−25% at 500-row
  scale; scales further at larger tables). Item 44: `Heap::delete_many` groups
  row_ids by page_id — one WAL mini-txn per page; WAL B/row 230 → 107 (−53%),
  416k rec/s at 5000 rows. Crash harness 38/38. Clippy/fmt clean.

- **A3 gate size-aware selectivity (item 43) — SHIPPED 2026-07-15, branch
  `43-a3-gate-size-aware`, PR #115 (DO NOT MERGE without independent bench
  validation run).**
  Four changes: (1) `page_count` in `TableStats` populated by ANALYZE; (2)
  size-aware cost model in `index_lookup_is_selective`; (3)
  `find_best_indexable_btree_predicate` picks the most selective AND arm; (4)
  gate added to `exec_select`. Follow-up fix: `parallel: bool` param added —
  `exec_select` passes `true` (parallel path, const=0.012), `matching_rows`
  passes `false` (serial path, const=0.05). This keeps 50%-selective DELETE on
  the scan path at ALL table sizes. 5 permanent tests in `tests/a3_gate.rs`.
  38/38 crash harness. Full bench re-run needed before merge.
- **Bench harness buffer-pool fix (item 42) + PK/FK relational-integrity
  stress bench (item 39) — SHIPPED 2026-07-15, branch
  `39-pk-fk-relational-stress-bench`, PR #111.**
  Found while generating a full-scale report to verify item 39's Table 5:
  `benches/decompose.rs` never sized its buffer pool (all 18 `Engine::open()`
  call sites used the library default), so any report sweeping into 1M+ rows
  silently hit `BufferPoolFull` and understated unidb's real throughput —
  measured 1,228 rec/s vs the true 15,905 rec/s at 1M rows (~13× recovered)
  after adding `bench_engine_open()` (2,000,000-frame pool, mirrors the
  `unidb-studio` demo fix). Item 39 itself: new Table 5 in the multi-model
  report — a real `customers`/`orders` PK/FK schema, made fair by item 36
  (FK row-level enforcement). Both correctness proofs pass on both engines
  (non-existent-parent INSERT rejected, still-referenced-parent DELETE
  blocked/RESTRICT). Full report re-run small-sweep for turnaround
  (`docs/performance/multi_model_report_20260715_091035.md`, 62 MiB peak
  RSS, all 5 tables). No `FORMAT_VERSION` bump, bench/docs scope only.
- **NEAR() vec_distance virtual column (item 41) — SHIPPED 2026-07-14, branch
  `claude/near-vec-distance-docs-ysqyvn`.**
  `exec_select_near` (`src/sql/executor.rs`) already computed the exact
  re-ranked Euclidean distance for every `NEAR` candidate to sort it, but
  never exposed it — `SELECT id, vec_distance FROM t WHERE NEAR(...)` returned
  `COLUMN_NOT_FOUND`. New `project_row_near` helper substitutes the reserved
  virtual column name `vec_distance` with the computed `Literal::Float`
  distance; `SELECT *` never includes it; outside a `NEAR` predicate the
  existing column lookup already returns `COLUMN_NOT_FOUND` (no code change
  needed for that half). 3 new tests in `tests/vec_distance.rs`. No
  catalog/API/format change. Spec's `vector_demo.py` acceptance item corrected
  inline — no such file exists anywhere in this repo.
- **B-tree index sort-then-bulk-load (item 40) — SHIPPED 2026-07-15, branch
  `40-btree-bulk-build`, PR #107 (MERGED).**
  `CREATE INDEX USING BTREE` on 540k rows: 134.2 s → 12.0 s (11.2×). Fix:
  collect (key, row_id) pairs, sort, `DiskBTree::insert_many` (one WAL
  mini-txn / one fsync). P40 crash test added (38/38). No FORMAT_VERSION bump.
- **Default buffer-pool capacity raised 4096 -> 65536 frames — 2026-07-14,
  branch `bump-default-buffer-pool-capacity`, PR #105.**
  Found via `unidb-studio` demo debugging (post items 35/36): the old default
  (32 MiB) is exhausted by a single ~30k-row table, and `fetch_page_for_write`
  forces a synchronous `wal.sync()` on every write once full
  (`BufferPoolFull`) — throughput collapsed to ~1-2k rows/s, indistinguishable
  from a regression. Corrected an initial Postgres-`shared_buffers` mental
  model along the way: unidb is mmap-backed, so the pool is pin/dirty-tracking
  metadata (~24 B/frame), not a page-data cache — page bytes already live in
  the OS page cache. New default (65536 frames = 512 MiB ceiling, ~1.5 MiB
  actual cost) chosen as a modest, measured bump (matches P1.c's own
  256->4096 precedent) rather than jumping to a huge number, because the frame
  table is allocated **eagerly** at open — a huge default would tax every
  `Engine::open()`, including ~50 test files and tiny embedded use. Follow-up
  backlog item filed for lazy/growable frame allocation, which would remove
  that tradeoff. No `FORMAT_VERSION` bump.
- **FK row-level enforcement (backlog item 36) — SHIPPED 2026-07-14, branch
  `36-foreign-key-row-enforcement`, PR #103.**
  Child INSERT/UPDATE verifies referenced parent key via `unique_index_root`
  DiskBTree (O(log n), item 35); heap-scan fallback for composite FKs. Parent
  DELETE/UPDATE enforces RESTRICT — rejected when a visible child references
  the key. `RecordKind::FkKey` phantom lock prevents concurrent parent-delete /
  child-insert race (10/10 PASS at CONC_REPEATS=10). NULL FK values unchecked.
  Same-txn parent+child insert works via own-xid visibility. 9 new constraint
  tests; 37/37 crash tests. No FORMAT_VERSION bump.
- **Unique-index enforcement (backlog item 35) — SHIPPED 2026-07-14, branch
  `35-unique-index-enforcement`, PR #102 MERGED.**
  `enforce_unique()` rewritten: implicit `DiskBTree` auto-created per
  `PRIMARY KEY`/`UNIQUE` column (INT64/TEXT/BOOL) at `CREATE TABLE` time;
  O(1) point lookup + MVCC re-check replaces O(n) heap scan per row.
  `unique_index_root: Option<PageId>` in `ColumnDef` with `#[serde(default)]`
  (no `FORMAT_VERSION` bump). UPDATE path maintained in `stage_row_index_writes`.
  Concurrent-INSERT PK race fixed: `RecordKind::UniqueKey` phantom lock
  (WaitPolicy::Wait) acquired before snapshot in `exec_insert` — serializes
  racing writers on same key. `pk-unique-race` conc_matrix cell (6w × 20rounds,
  CONC_REPEATS=10): 10/10 PASS. P35 crash test (37/37 total). 6 regression tests.
  Measured: PK INSERT flat ~27-30k rec/s (was 5k→1k/s O(n²)).
- **CDC Management API (backlog item 33) — SHIPPED 2026-07-14, branch
  `33-cdc-management-api`, PR #96 (MERGED).**
  Three new JWT-protected routes: `GET /tables/{name}/events` (CDC status —
  `{ "enabled": bool }`; 404 if table absent), `DELETE /tables/{name}/events`
  (disable CDC — idempotent 204), `GET /events/head` (current max committed seq
  in `__events__`, O(1) via seq DiskBTree, or `{ "seq": 0 }` if empty).
  Engine: `is_events_enabled`, `disable_events` (mirrors `enable_events`, same
  catalog-write path), `events_head_seq` (DiskBTree::max_entry). P34 crash test
  added (36/36 pass). 6 new integration tests (10/10 total for server_events).
  Workspace tests all green. Clippy/fmt clean.
- **Observability API gaps (backlog item 34) — IN PROGRESS 2026-07-14, branch
  `34-observability-api-gaps`, PR #97 open — STOP for review.** Part A: `UNIDB_SLOW_QUERY_MS`
  env var + `PUT /config/slow_query_threshold_ms` (superuser-gated); wires the
  existing `set_slow_query_threshold` atomic. Part B: `StatsPoint` ring buffer
  (300 points, `Mutex<VecDeque>`); `src/stats_ticker.rs` background thread
  (autovacuum pattern: Weak<Engine>, condvar, bounded-join); `GET /stats/history`
  returns oldest-first points with server-side rate fields. Gates: crash 36/36
  unchanged (item 33 added P34; item 34 adds none); workspace tests 0 failures;
  clippy/fmt clean.
- **Bulk Load HTTP API (backlog item 32) — SHIPPED 2026-07-14, branch
  `32-bulk-load-api`, PR pending (STOP-for-review — NOT merged).**
  `POST /tables/{name}/bulk`: JWT-protected NDJSON body, one transaction for the
  whole body (begin once, `prepare` once, loop `execute_prepared`, commit once) —
  amortizes the HTTP/fsync envelope, NOT B-tree cost.
  **Throughput CORRECTED (2026-07-14) — an earlier "~60–87k rows/sec" claim was
  ~2.5–5× inflated and unbacked; the reproducible `#[ignore]`d
  `bulk_throughput_measurement` (release, server `elapsed_ms`) measures
  ~12k–31k rows/sec** (no-index 17k@100k→31k@200k, with-index 17k→12.5k as the
  B-tree grows) = **~20–50× over the ~640/sec per-row path but BELOW the
  50k–200k target.** SQL-path per-row cost (JSON parse + coercion +
  `execute_prepared`) on top of the ~30 µs/row engine insert bounds it; reaching
  50k+ needs a lower-level path (filed follow-up: channel-streamed body → raw
  bulk loop, parallel apply, optional `?chunk=N`). V1 buffers ≤512 MiB before the
  txn (NDJSON validated up-front). No format/WAL/recovery/engine change. Files:
  `src/server/bulk.rs`, `EngineHandle::bulk_insert`, `tests/server_bulk.rs`
  (10 correctness + 1 ignored throughput), `benches/server.rs`. Crash harness
  **35/35** unchanged; sync invariant clean; clippy/fmt green. Docs: REST_API.md,
  spec (measured-result correction), backlog_index row 32, PROGRESS.md — all
  carry the honest ~12–31k number.
- **Studio API readiness (backlog item 30) — SHIPPED 2026-07-14, branch
  `30-studio-api-readiness`, PR TBD (STOP-for-review).** E1 (G9): `Expr::Like`
  + `QExpr::Like` added to both expression paths (single-table fast path and
  multi-table planner path); `like_match()` Unicode-correct pattern matcher
  (`%` = any run, `_` = one char, NULL propagation); ILIKE = `case_insensitive:
  true`; differential-tested against rusqlite (`PRAGMA case_sensitive_like = ON`
  for LIKE; `lower(col) LIKE lower(pattern)` for ILIKE). E2 (G11): `Expr::Match`
  + `QExpr::Match`; `find_match()` + `exec_select_match()` over-fetch-then-filter
  via FULLTEXT DiskBTree (mirrors NEAR exactly); `plan_is_concurrent_read` updated
  to exclude MATCH; QExpr path uses inline tokenize check. `MATCH(col, 'text')` is
  now a usable WHERE predicate over `/sql`, no new REST routes. E3: §12 ERP app
  walkthrough added to `engine_access_guide.md` with concrete curl payloads for
  auth, schema+FK, ERD introspection, atomic multi-model txn (one WAL commit),
  realtime events, NEAR+MATCH search, LIKE record browser, cursor paging.
  23 new `tests/like_match.rs` differential + MATCH tests. Crash harness:
  **35/35** (unchanged). All gates green: build, workspace tests, clippy, fmt.
- **Multi-page catalog (backlog item 25) — SHIPPED 2026-07-13, branch
  `25-multipage-catalog`, PR #73 (MERGED).** `Catalog::persist`
  chains the JSON blob across N pages (4-byte magic + 4-byte next_page_id
  chain header per page, 8128 bytes JSON per 8 KiB page); `Catalog::load`
  detects magic vs. legacy raw JSON. One mini-txn covers all N chain pages;
  `catalog_root` flip is the atomic commit point. No `FORMAT_VERSION` bump;
  old single-page blobs open unchanged. Crash point P33 added (35/35).
  Before: HeapFull at ~8.1 KiB (item-23 original layout hit HeapFull{8883});
  after: unlimited schema size. 4 new catalog unit tests + 4 integration tests +
  P33 crash test. Docs: spec → SHIPPED; backlog_index row 25 → ✅;
  storage_service.md §4 ceiling note; engine_design.md §4.6 + footer;
  PROGRESS.md entry. **PR #73 merged.**
- **Subscription CDC — canonical envelope, before/after, format adapters, lag
  observability (backlog item 29) — SHIPPED 2026-07-13, branch
  `29-subscription-cdc`, PR #72 (MERGED).** C1: `before`/`after`/
  `ts_ms` row images in every CDC event; canonical envelope in `__events__.payload`
  back-compat with old flat events. C2: Debezium + Supabase format adapters via
  `?format=` on SSE subscribe (`src/server/event_format.rs`). C3:
  `unidb_catalog.subscription_lag` virtual relation + `/stats` JSON +
  Prometheus `unidb_subscription_lag_events{consumer}` /
  `unidb_subscription_lag_seconds{consumer}`. C4: guide §8 (§8.1–§8.6)
  updated with contract, three format examples, and lag detection guidance.
  Gates: workspace tests all green (crash 33/33 unchanged); clippy/fmt clean.
  **PR #72 merged.**
- **Replication time-PITR + logical replication (backlog item 28) — SHIPPED
  2026-07-13, branch `28-replication-time-pitr`, PR #70 (MERGED).**
  R1 (MUST): `src/backup/timeline.rs` — `TimelineIndex` appends one 16-byte
  `(ts_micros, lsn)` mark per user-txn commit after WAL sync. WAL format
  unchanged (no FORMAT_VERSION bump, no §3/D9 sign-off). `backup::restore_to_time`
  + `Engine::restore_to_time` free function resolve wall-clock → LSN; `archive_wal`
  also archives `timeline.bin`. Crash point P31 (torn timeline mark → silently
  skipped, PITR falls back to prev mark). R2 (SHOULD): new workspace crate
  `unidb-logical` (wraps item-20 `Dispatcher` + `LogicalApplySink`); translates
  events to INSERT/UPDATE/DELETE SQL on a target `Engine`; at-least-once,
  offset-durable, survives primary restart. Known gap (UPDATE old key) filed as
  item-26 follow-up.
- **Event queue at scale (backlog item 26) — SHIPPED 2026-07-13, branch
  `26-event-queue-scale`, PR #68 (MERGED).** Q1: durable
  `DiskBTree` secondary index on `__events__.seq`; `poll_events` /
  `poll_events_after` now O(log n + returned) via `search_range_limit` +
  MVCC re-check. Flat-latency proven (`benches/poll_events.rs`): 10k→30 µs,
  100k→28 µs, 300k→36 µs with limit=20. Q2: `EventWake` condvar (committed
  in Engine after WAL sync, P5.e-compliant); `Engine::commit` notifies;
  SSE route blocks on `wait_event_commit` instead of spinning; dispatcher
  builder takes optional `event_wake`. Q3: `vacuum_events` removes seq index
  entries on reclaim — index never pins retention. Crash point P30 added
  (seq index torn mid-append; reopen recovers); crash harness 32/32. Gates:
  `cargo test --workspace --features server` all green (385 + 32 crash +
  other workspace crates); clippy `--workspace --all-targets -D warnings`
  clean; `fmt` clean; conc-matrix 28/28 (1 repeat). Docs: `engine_design.md`
  §6.2 + §6.3 + tech-debt corrected, `26_event_queue_scale.md` → SHIPPED,
  `backlog_index.md` row 26 → SHIPPED, `PROGRESS.md` entry. **Next: await
  PR review.**
- **Per-table vacuum accounting, cost throttle (backlog item 27) — SHIPPED
  (2026-07-13), branch `27-vacuum-per-table`, PR #69 (STOP-for-review).**
  V1 (per-table dead/live estimates via `per_table_estimates: Mutex<HashMap>`),
  V2 (`Engine::vacuum_table(name)` — scoped single-table pass using M10 logic),
  V3 (cost throttle: `VacuumCostConfig` + `VacuumThrottle` napping when
  cost_limit is spent). Autovacuum worker now calls `vacuum_table` per triggered
  table (`tables_needing_vacuum`). **V4 (whole-table compaction) deferred** —
  re-pointing every index entry for relocated tuples requires a new multi-page
  WAL record type (FORMAT_VERSION concern). **Measurements:** 200 rows × 10
  churns = 2000 dead; `vacuum_table("hot")` reclaims 2000, cold table = 0
  untouched; throttle (cost_limit=50, 2ms) adds ~10× overhead vs unthrottled
  (expected; default cost_limit=200 → ~2.5×). **Tests:** 7 new unit tests + P31
  crash test (crash mid-vacuum_table). **Gates:** crash 33/33 (+1 P31),
  workspace tests all green, clippy/fmt clean, no FORMAT_VERSION bump, no §3
  decision reopened.
- **Object storage service (backlog item 23) — SHIPPED + MERGED (2026-07-13),
  branch `23-storage-service`, PR #64.** New
  **app-layer** crate `unidb-storage` (workspace member; adds **no engine
  surface**, keeps tokio + `aws-sdk-s3` out of the engine's sync build). Bucket/
  object **metadata** in ordinary unidb tables (`buckets`, `objects`,
  `object_dlq`); object **bytes** tiered: `< inline_threshold` (1 MiB) → engine
  **LOB in the same txn as the metadata row** (ACID; commit/rollback proof); ≥ →
  an **S3-wire store** — one `S3ObjectStore` (aws-sdk-s3) for **both** MinIO
  (dev) & S3 (prod), selected by config (`STORAGE_BACKEND`), plus a
  `MemoryObjectStore` Docker-free test double. Large-object consistency = an
  **outbox** (`objects` insert event commits atomically with the pending row,
  events enabled on `objects`) + a **`Reconciler`** that confirms
  (`pending→ready`), compensates (`pending→failed` + compact-DLQ row, never a
  dangling pending), and **sweeps orphaned bytes**. **Presigned PUT/GET** move
  browser bytes directly — engine never proxies large payloads (§10).
  **Design-note decisions** (`docs/design/storage_service.md`): (1) `aws-sdk-s3`
  over object_store/rusoto for **offline SigV4 presigning** + MinIO
  endpoint/path-style control; (2) confirm/compensate authority is a
  **reconciler keyed on `created_at` age**, NOT the item-20 Dispatcher's tight
  in-cycle retry (documented **wall**: ms retry ≠ upload grace window) — genuine
  item-20 reuse remains via an optional `ConfirmSink` on a real
  `Dispatcher`+`Filter`. **Engine constraint surfaced & worked around (NOT an
  engine change):** unidb persists the whole catalog as **one ~8 KiB page blob**;
  `objects`+`storage_key`+the 8-col dispatch DLQ overflows it
  (`HeapFull{size:8883}`), and a *runtime* `CREATE TABLE` re-serializes a
  row-volume-grown catalog and overflows too → dropped the **derivable**
  `storage_key` column, used a compact 4-col `object_dlq`, and moved **all DDL
  up front into `StorageService::new`** (reconciler does zero DDL). Verified at
  scale (`tests/scale.rs`: 1 000 objects + reopen, no overflow). **Gates:**
  `cargo test --workspace` green (storage: 3 crash + 4 round-trip + 1 outbox + 4
  presign/config + 1 scale = 13); crash harness **31/31 unchanged**; `clippy
  --workspace --all-targets -D warnings` + `fmt` clean; sync invariant intact.
  Docker: `docker/docker-compose.minio.yml` (dev); live test gated behind
  `STORAGE_S3_ENDPOINT`. Docs: crate README, engine_access_guide §10, design
  note + design_index, backlog index row 23, spec→SHIPPED, PROGRESS "Object
  storage service (item 23)". **Studio "Storage" tab = out of repo.**
  **Follow-up filed:** item 25 (`25_multipage_catalog.md`, Improvement, NOT
  STARTED) — lift the single ~8 KiB catalog-blob ceiling (table defs + stats)
  this work hit; extends item 10; once shipped, the storage-layer workaround
  (compact schema, DDL up front) can be relaxed.
- **Logs surface (backlog item 22) — SHIPPED (2026-07-13), branch
  `22-logs-surface`, PR pending. STOP-for-review (do not merge).** Made the
  server's structured logs queryable + shippable without a log database.
  **L1:** `unidb-server` logs **JSON lines** to stdout + rolling `unidb.log.*`
  files (`UNIDB_LOG_FORMAT=text` opt-out); enabled via `tracing-subscriber/json`
  **under the `server` feature only** — default build dep graph unchanged.
  **L2 correlation:** middleware (`server/correlation.rs`) assigns a
  `request_id` before auth, scopes it as a tokio **task-local**, enters an
  `http_request` span, echoes `x-request-id`. `EngineHandle`'s `spawn_blocking`
  choke points copy the task-local into an engine-core **thread-local**
  (`src/observability.rs` — default build, `std`-only, no new dep) that the
  synchronous slow-query log + `audit.log` read; `Engine::execute_sql` wraps a
  span tagged `txn_id`/`request_id`. `audit.log` records gained
  `txn_id`+`request_id` + an app-log mirror. So one request's lines join across
  app log, slow-query log, audit log by `request_id`. **L3:** `GET /logs`
  (`server/logs.rs`, superuser-gated via `ensure_superuser`) — bounded,
  cursor-paged **reverse read** of the JSON files: page cap 500, per-request
  scan budget 50 000 lines, 64 KiB reverse blocks (never loads a file whole),
  filename+offset opaque cursor. Proven not to OOM/stall on a huge dir
  (`tests/server::logs::scan_budget_bounds_work_on_a_needle_in_a_haystack`:
  >55k-line file, match only at oldest end → scans exactly the budget, returns
  resume cursor, needle reachable by paging). **L5:** `ops_runbook.md` §8 =
  CW/Datadog/Loki agent configs (the JSON files are the shipping contract).
  **L4** (studio Logs tab) out of repo — noted only. **Metrics:** JSON-overhead
  ladder within noise (bare 280 / text 233 / json 282 commits/s @ real fsync —
  per-commit fsync dominates, format is noise; acceptance "ladder within noise"
  met); no perf headline (observability surface); RSS unchanged. **Files:** new
  `src/observability.rs`, `src/server/{correlation,logs}.rs`, `tests/{server_logs,
  logs_correlation}.rs`; touched `audit/mod.rs`, `lib.rs` (execute_sql span +
  slow-query enrich + audit call sites pass xid), `server/{engine_handle,router,
  handlers,mod,error}.rs`, `bin/unidb-server.rs`, `Cargo.toml`. **Gates green:**
  `cargo test` default **380 + crash 31/31**; `--workspace --features server`
  green (incl. new server_logs 3 + logs_correlation 1); clippy `-D warnings`
  clean (default + server `--all-targets`); fmt clean. No §3 decision reopened,
  no on-disk format change, no crash-point change. Docs: REST_API `GET /logs`,
  ops_runbook §8, README, engine_design (§8 + module map + footer), backlog
  index + item-22 doc → SHIPPED, PROGRESS entry. **This closes item 22 (L1–L3,
  L5); L4 is studio-side.**
- **Observability metrics enrichment (backlog item 21) — SHIPPED (2026-07-13),
  branch `21-observability-metrics`, PR #62.** Enriched the P6.g
  observability surface with production-grade metrics captured **lock-free** at
  existing chokepoints, surfaced only via `Engine::stats()`/`GET /stats` +
  Prometheus `/metrics` (no new endpoint). New `src/metrics.rs` = a lock-free
  `AtomicHistogram` (48 power-of-two buckets, `record` = 3 `Relaxed` fetch_adds;
  `le`-convention percentile **estimates** read on the cold `stats()` path) +
  counter snapshots. Capture sites: per-statement-kind latency
  (`lib.rs::execute_one_plan`), WAL-fsync latency+count (`wal.rs::sync`/
  `group_fsync` around `sync_all`), buffer-pool hit/miss/evict
  (`bufferpool.rs::fetch_page`/`find_victim`), lock-wait dur/count + deadlocks
  (`lockmgr.rs::acquire` blocking path), the alertable **vacuum-horizon-age
  gauge** (`txn.rs` — each live writer/reader carries a begin `Instant`;
  `oldest_snapshot_age()`), per-table heap page counts (cold FSM-dir walk in
  `stats()`), parallel-worker utilization vs `GLOBAL_MAX`
  (`sql/parallel_scan::acquire`), and server session gauges (open sessions/
  cursors + idle-reaper aborts, merged in `get_stats`). **No mutex on the
  commit/scan path.** Horizon-age proven by
  `txn::tests::horizon_age_grows_while_rr_idle_and_resets_on_commit` (idle RR
  grows it; commit/abort resets to 0). **Overhead A/B (HEAD vs `main`@842bb12
  clone, quiet machine, single bench process, PG off):** single-threaded
  `mmreport` Table 3.1 within ±1% at scale (bulk insert −0.65%/−0.86% @1M/2M;
  full-scan select ±0.28% @1M/2M — the buffer-pool-atomic path); W0→W4 ladder
  indistinguishable; Table C 8-writer ~2–3% mean but fully inside its ±8%
  run-to-run band (distributions overlap) → noise, not regression. **Honest
  limitation:** per-table *dead-tuple* estimate stays engine-global (estimators
  are global counters) — table-health widget uses real per-table `pages` +
  engine-global dead-tuple pressure. Gates: `-p unidb --features server` +
  `--workspace` pass, crash **31/31**, conc-matrix **28/28** (toggle on+off, 18
  spinners), clippy/fmt clean. No `FORMAT_VERSION` bump, no §3 decision reopened.
  Widget-traceability table: `docs/engine_access_guide.md` §9. See PROGRESS
  "Observability metrics enrichment (item 21)".
- **Events / realtime dispatcher (backlog Milestone 20) — SHIPPED (2026-07-13),
  branch `20-events-dispatcher`, PR pending.** Makes M4's atomically-captured
  event stream consumable downstream **without any new engine application
  shape** (M18 boundary holds). **E1 (framing only):** `GET /events/subscribe`
  gained an ephemeral live-tail mode (no durable consumer) resuming from the
  standard SSE `Last-Event-ID` header / `?from_seq=` / `?table=` filter, backed
  by one **read-only** engine method `poll_events_after(after_seq, limit)`
  (`src/lib.rs`, also on `server::engine_handle`); durable-consumer at-least-once
  mode unchanged. **E2:** new workspace crate **`unidb-dispatch`** (`src/{lib,
  sink,filter,dlq}.rs`) — embeds `Arc<Engine>`, polls a durable offset, fans out
  to `WebhookSink` (retry→**dead-letter table dogfooded into unidb**) + `RoomSink`
  (broadcast rooms) with per-sub table/op filter + column projection, then acks;
  at-least-once, crash/replay zero-loss. **Justification for own crate (not a
  server module):** keeps `tokio`/`reqwest` OUT of the `unidb` crate — `cargo
  tree -p unidb --no-default-features --edges normal` shows no async runtime, so
  "engine stays sync" is literally true. **E3:** event-schema + replay/
  vacuum-horizon contract in `docs/engine_access_guide.md §8`. **E4 (studio tab)
  = out of repo.** Acceptance proven: `unidb-dispatch/tests/dispatch_delivery.rs`
  (I/U/D once-each, resume-from-durable-offset with **zero loss across
  drop+reopen**, crash-between-deliver-and-ack **redelivers**) +
  `dispatch_webhook_dlq.rs` (500-endpoint retried 3×→dead-lettered, offset still
  advances) + `tests/server_events.rs` ephemeral-resume tests. **Honest caveat
  (§0.6):** inherits M4 `poll_events` O(total-events)/no-`seq`-index cost →
  ≈O(N²/limit) drain; fast through N=4k (~95–120k ev/s drain; ingest fsync-bound
  ~300 ev/s), bites at large backlog — fix = engine-side `seq` index (M4 tech
  debt, not opened). Dispatcher pins the vacuum horizon if it falls behind →
  `CycleReport.backlogged` + `WARN`. **No `FORMAT_VERSION` bump, crash harness
  stays 31/31, no §3 decision reopened, moat framing respected** (events stay
  ordinary durable rows; dispatcher consumes the table, not the WAL). Gates
  (post-rebase onto item 22): `-p unidb` 380 + `--features server` (all bins,
  incl. item-22 server_logs/logs_correlation) + `-p unidb-dispatch` (6+4) all
  green, crash 31/31, clippy/fmt clean, sync invariant (no tokio in engine). See
  PROGRESS "Events / realtime dispatcher (Milestone
  20)", `docs/backlog/20_events_realtime_dispatcher.md`.
- **Engine access & introspection contract (backlog Milestone 18) — SHIPPED
  (2026-07-13), branch `18-engine-access-contract-impl`, PR pending.** Delivered
  a SQL-queryable **system catalog** as synthesized virtual relations over the
  ordinary query surface — `information_schema.{tables,columns,table_constraints,
  key_column_usage,referential_constraints}` (C1–C3) + `unidb_catalog.indexes`
  (C4) — in `src/sql/information_schema.rs`. Routing: reserved names resolve to a
  fixed synthetic schema in `sql/plan.rs::plan_from`; rows materialize from the
  live catalog in `sql/query_exec.rs::Runner::scan`; the parser forces a SELECT
  over an introspection relation onto the Phase-4 Query path so one virtual-scan
  impl serves single-table *and* multi-way JOINs; the two `COUNT(*)` parallel
  fast paths are guarded against `Heap::open` on a virtual relation. **Pure
  read-side projection** — FK/PK/UNIQUE/CHECK already parse+persist (M11), so **no
  catalog schema change, no `FORMAT_VERSION` bump, no crash surface (harness stays
  31)**. Constraint names synthesized Postgres-style (`<t>_pkey`/`_key`/`_fkey`/
  `_check`), stable across reopen. C5 object-DDL = documented reconstruction rules
  (no stored `CREATE` text, no table-function syntax). **Honesty notes:**
  `JOIN…USING`/`NATURAL` unsupported → worked-example ERD query rewritten to
  equivalent `ON` form (composite-key alignment via `ordinal_position =
  position_in_unique_constraint`); FK is metadata-only (M11 referenced-table
  existence, `update/delete_rule = NO ACTION`); no `unidb://` DSN parsed (attach =
  base URL + Bearer JWT, one db/server). Docs: new `docs/engine_access_guide.md`
  (Application Builder's Guide — A1/A2/B1–B4/C/D1/D2/E1, "schema explorer in 30
  lines"), linked from `documentation_index.md`; `GET /tables` marked
  superseded-but-kept in REST_API.md; engine_design module map + footer, README,
  backlog_index row 18, PROGRESS entry all updated. **Parity proven** across embed
  (`tests/information_schema.rs`), attach
  (`unidb-attach/tests/attach_sql.rs::information_schema_fk_join_over_attach`), and
  server `/sql` (`tests/server_sql.rs::information_schema_over_sql_route`) — same
  query, same rows; differential test runs the 4-way ERD join over a composite
  PK/FK schema. Gates green: `-p unidb` (default + `--features server`), crash
  **31/31**, `--workspace --features server`, clippy `-D warnings`, fmt. No §3
  decision reopened. See PROGRESS "Engine access & introspection contract
  (Milestone 18)", `docs/backlog/18_engine_access_contract.md`.
- **`UNIDB_CONCURRENT_SQL_WRITES` default-ON flip (backlog item 11 follow-up) —
  SHIPPED (2026-07-13), branch `11-concurrent-writes-default-on`, PR pending.**
  The concurrent SQL-write path (catalog-lock split 0a/0c + latch-coupled
  "crabbing" `DiskBTree` descent) soaked dark behind the default-off toggle; its
  soak blocker was item 16 (fixed PR #50), and the 28-cell concurrency matrix now
  passes **28/28 at `CONC_REPEATS=10`** toggle on AND off. So the default is now
  **ON**. Mechanism: `env_flag` → `env_flag_default_on` (unset ⇒ on; only
  `0`/`false`/`off`/`no` force off); `Engine::set_concurrent_sql_writes(false)`
  still overrides at runtime; toggle-off regression test + serialized `cat_write`
  path stay compiled in. **Revert is one env var** (`UNIDB_CONCURRENT_SQL_WRITES=0`).
  **Table C re-measured on the flipped default (no env):** indexed 8-writer
  **811 → 1016 commits/s (+25%** on this machine; original ship was +38%,
  768 → 1058 — same mechanism/direction, absolute varies by machine); `=0`
  override drops back to ~741–811 (serialized). Peak RSS ~31.4 MB (bench process,
  unchanged by the flip). Gates: `-p unidb` + `--features server` + `--workspace`
  pass, crash **31/31**, clippy/fmt clean, matrix 28/28. No §3 decision reopened,
  no format change. Docs closed out (README, engine_design §5.2/§5.4 + footer,
  processing-engines notes, high_scale_concurrency, backlog index + item-11/16
  docs, conc_matrix legend). See PROGRESS "UNIDB_CONCURRENT_SQL_WRITES default-ON
  flip". **This closes item 11's filed follow-up.**
- **MVCC visibility anomaly under concurrent SQL writes (backlog item 16) —
  ROOT-CAUSED + FIXED (2026-07-12), branch `16-visibility-fix`, PR pending.**
  **Root cause (one bug, all three symptom classes):** `TransactionManager::
  abort` (`src/txn.rs`) removed the aborting xid from the `active` set **before**
  physically undoing its heap writes. Visibility has no "aborted" state
  (`mvcc::is_committed_at_snapshot` = not-active-and-below-`next_xid` ⇒
  committed), so during the undo window a concurrent snapshot saw the aborting
  txn's doomed new UPDATE version as committed (visible) and its superseded old
  version as invisible — a wrong reader result, and (since the new version's
  RowId is unlocked — `heap.update` locks only the *old* version) a concurrent
  writer could chain onto it, after which undo restored the old version ⇒ **two
  live versions of one id (persistent duplicate) or none (missing row)**. The D5
  flush error and the >120 s hang were **downstream** of this corruption, not
  separate bugs. **Fix (single-site):** keep the xid `active` (and its row locks
  held) through the whole physical undo; drop from `active` / mark aborted /
  `release_all` only after. `commit()`'s early remove-from-active is intentional
  and correct (its data *is* committed) — only `abort` needed reordering.
  **Evidence:** deterministic `txn.rs::
  aborting_txn_new_version_never_visible_to_concurrent_snapshot` (barrier pins an
  observer scan to the abort midpoint — pre-fix reads doomed `"v2"`, post-fix
  `"v1"`) + `tests/concurrent_writers.rs::
  item16_readers_during_cross_row_churn_{off,on}` (8w×8rows+2r, fails pre-fix
  without external load — lost/gained row, COUNT disagree, >90 s hang — passes
  post-fix). **Matrix: 17 PASS/11 FAIL → 28 PASS/0 FAIL** at `CONC_REPEATS=10`,
  18 spinners, toggle off AND on. Gates: lib 374 + all integration green, crash
  harness **31** (unchanged — recovery undo is single-threaded, window never
  exposed), clippy/fmt clean. Peak RSS ~9.7 MB (buffer-pool bounded). **No §3
  decision reopened (D5 not touched).** Item 11's default-ON flip is now
  unblocked on correctness. See `docs/backlog/16_…`, PROGRESS "MVCC visibility
  anomaly under concurrent SQL writes", engine_design §4.1/§4.3.
- **Concurrency correctness matrix (item-16 tooling) — ADDED (2026-07-12),
  branch `conc-correctness-matrix` (bench + scripts + docs only; NO engine
  code touched).**
  `benches/conc_matrix.rs`: 28 production-shaped concurrent read/write
  correctness cells (insert storm · cross-row UPDATE churn = the item-16 shape ·
  same-row contention · mixed CRUD · readers-during-churn at RC/RR/SER ·
  parallel-scan readers · balance-transfer sum invariant · vacuum×churn ·
  delete+reinsert) × `UNIDB_CONCURRENT_SQL_WRITES` on/off × indexed/unindexed,
  with pass/fail oracles (exact visible-id set, no dup ids in any snapshot,
  COUNT(*) agreement, repeatable re-reads, sum invariance, index-vs-scan
  agreement), CPU-contention spinners, per-cell repeats, and a per-repeat hang
  deadline (a deadlock becomes a FAIL row, worker abandoned, matrix continues).
  `scripts/report.sh` now appends the matrix table to EVERY report (both
  modes, native execution) and gained `--conc` (matrix-only →
  `docs/performance/conc_matrix_<ts>.md`, git-ignored) + CONC_* knobs
  (`scripts_guide.md` updated).
  - **⚠ FINDINGS (release, native macOS M5 Pro, commit `0c09a70`) — item 16 is
    WORSE than filed and NOT toggle-gated.** Toggle **OFF (production
    default)**: transfer-sum readers see a short/torn RC statement snapshot
    **7/10** runs; vacuum racing cross-row churn leaves **persistent duplicate
    visible ids after quiescence + final vacuum** **3/10**; 8w×8rows cross-row
    churn leaves post-quiescence duplicates **1/6**. Toggle **ON**: same shapes
    to **10/10**; a **D5 violation at commit** (`Recovery("D5 violation on
    flush: page LSN > durable WAL LSN")`) in 8w indexed churn; one run **hung
    >120 s** (deadlock/livelock) under spinner contention. 2w×2rows (the
    original test geometry) passes without spinners — the shipped test was too
    small to catch this reliably; **with spinners it fails 2/3 too.** Official
    full-matrix run (3 repeats, 18 spinners): **17 PASS · 11 FAIL of 28** —
    toggle-off FAILs: reader `COUNT(*)=7` mid-churn (2/3), transfer-sum short
    RC snapshot (1/3); toggle-on adds reader `COUNT(*)=9` (the extra-row
    signature), RR/SER readers missing a live row, a parallel-scan reader
    missing a row, vacuum×churn persistent duplicates.
    Recorded in `backlog_index.md` item-16 entry (update block)
    and `index_write_concurrency.md` known-issue section (results table +
    focused repro commands). **Item 16 root-cause is now the top backlog
    priority; symptom family = scan concurrent with cross-row-UPDATE commit
    sees superseded version (dup id) or misses the live row (short scan).**
    _(RESOLVED 2026-07-12 — root cause was abort dropping the xid from `active`
    before physical undo; fixed in `txn.rs`. See the top "Current status" entry.)_
    Note: PR #45's body says "backlog item 16" in places — stale labels from
    before that work was renumbered to **17**; PR #45 is item 17
    (replaced-stack headline), unrelated to this anomaly.
- **Cross-domain headline vs the replaced stack (backlog item 17) — SHIPPED
  (2026-07-11), branch `mm-replaced-stack-headline`, PR pending.** Made §6 Table 4
  honest: it *claimed* "one atomic txn vs the replaced stack" but compared unidb's
  4-model commit against a single PG relational row. Added a real replaced-stack
  baseline (`pg_replaced_stack_throughput`) — the same four writes as four
  independent PG commits (row + pgvector+HNSW + graph adjacency + outbox), no
  shared txn — behind `MM_REPLACED_STACK=1`. **Result: unidb's one atomic commit
  is 3.61× faster under real flush-to-platter fsync** (F_FULLFSYNC vs
  fsync_writethrough, 250 vs 69 txns/s); **~parity under Docker's cheap VM fsync**
  (the win is durability-cost-dependent — critical measurement-hygiene point).
  **Unconditional win: crash-consistency** — 0 orphans vs torn record, proven by
  two new `tests/crash` `item16_*` tests (harness 29 → **31**). Benches+docs only,
  no §3. HOT/A2 **deferred** (ROI vs §1). See [[unidb-moat-and-wal-model]],
  PROGRESS "Cross-domain headline", `docs/backlog/17_mm_replaced_stack_headline.md`.
- **REST API enrichment (backlog item 12) — SHIPPED (2026-07-11), branch
  `claude/rest-api-enrichment-vly934`, PR #43 (merged).** The last filed
  NOT-STARTED backlog item; **server-layer only** (engine gains just two
  delegating pub methods: `set_rls_policy_sql` — RLS policy parsed from a SQL
  predicate string via the ordinary parser, no `Expr` wire format —
  and `ensure_superuser`). No format bump, **crash harness untouched at 29**,
  sync invariant holds (`base64` is server-feature-gated).
  - **R1 transaction sessions:** `POST /txn/begin` (201: txn_id/isolation/
    expires_at) opens a real client-held txn; `/sql`, `/cypher`, `/rows`
    (+`/rows/batch`), `/edges` accept `X-Txn-Id` and don't auto-commit;
    `POST /txn/{id}/commit|rollback` finish. `server/txn_session.rs` registry
    enforces the spec's hard points: per-session busy try-lock (2nd concurrent
    request → **409 TXN_BUSY**), JWT-principal binding (**403**), **idle
    reaper** (Weak-ref tokio task; `UNIDB_TXN_IDLE_TIMEOUT_SECS` default 60)
    auto-aborts abandoned sessions → horizon un-pinned (verified via `/stats`),
    stale ids → **404 TXN_NOT_FOUND**. Sessions reject DDL (`DDL_IN_SESSION` —
    engine DDL rollback is request-scoped per P2.c, so allowing it would break
    session rollback); a failed mutating statement aborts the session
    (Postgres-without-savepoints); failed pure reads keep it open.
  - **R2:** optional `isolation` on one-shot `POST /sql` (rc/rr/serializable;
    takes the transactional path so the level governs). Write-skew over HTTP
    (session + one-shot serializable) rejected 409 — SSI participation proven.
  - **R3:** `POST /events/vacuum` (M4 all-consumers contract), superuser-gated
    `PUT /tables/{table}/rls` + `POST /admin/flush`.
  - **R4:** `POST /rows/batch` (base64, ≤10k rows/32 MiB, decode-validated
    before any insert, atomic, session-aware) and result cursors
    (`POST /sql {"cursor": true}` → `GET /sql/cursor/{id}?limit=`,
    principal-bound, idle-expiring; honest caveat documented: decoded rows
    stay buffered server-side — the sync executor materializes; the cursor
    bounds each response's JSON).
  - **Measured (release, Linux container, `benches/server.rs`
    `rest_enrichment`): 100 INSERTs 161.3 ms one-shot → 33.9 ms in a session
    (4.8×, 100 fsyncs → 1); 500 raw rows 718.4 ms singles → 35.0 ms batched
    (20.5×).** Peak RSS 43 MB.
  - +24 integration tests (`tests/server_txn.rs`, `tests/server_enrich.rs`,
    both registered with `required-features` — the #28 lesson); `ApiError` is
    now an enum (Db | server-layer Api codes). §9 staleness fixed in passing:
    `REST_API.md` intro (still described the retired writer-thread design) +
    error table (missing P5.d/P5.f/P6.b/P6.e codes); `engine_design.md`
    §8/§9/RLS/module-map/footer; README status/env/layout/attach notes.
  - **⚠ Found during verification, NOT caused by this work (reproduced on
    unmodified `main` @ dc93931): pre-existing MVCC visibility anomaly under
    `UNIDB_CONCURRENT_SQL_WRITES`** — `cross_row_update_deadlock_resolves_
    no_hang` under CPU contention (6 parallel test-binary instances) can end
    with 3 visible rows instead of 2 (~1–5/6 fail per round; always green in
    isolation, so per-PR gates never saw it). Filed: backlog_index "Next up"
    item 16 + a known-issue section in `index_write_concurrency.md`. **Blocks
    item 11's planned default-ON flip**; production default (off) unaffected.
  - Follow-ups filed: attach-client session support (optional); item 16 above.
- **Processing-engines design-doc collection — ADDED (2026-07-11), branch
  `claude/processing-engines-design-docs-dtcp16`, PR #42. Docs only — NO engine
  code touched; no format/crash/§3 impact.** New `docs/design/processing-engines/`
  (12 documents + index, registered in `docs/design/design_index.md`): per-engine
  deep dives (storage core, WAL & recovery, MVCC/txn, SQL, indexing, vector,
  graph, event queue, parallelism + benchmark/metrics analysis,
  server/replication/ops) with Mermaid architecture/flow diagrams, exact
  on-disk layouts, border-case tables, measured numbers distilled from
  `PROGRESS.md`, and a **proposal-status** future roadmap
  (`12_future_roadmap.md` — explicitly not authorization to start work;
  backlog conventions still apply). Updated on merge with `main` so docs 10
  and 12 reflect item 15 (parallel scan default-ON + worker governance).
- **Parallel worker governance (backlog item 15) — SHIPPED (2026-07-11), branch
  `parallel-worker-governance`, PR pending.** Closed the two real blockers that
  kept parallel scan default-off, then **flipped it default-ON**. This also
  explains why `report.sh` showed no parallel win — the bench never set
  `UNIDB_PARALLEL_SCAN`, so it ran serial; default-on now shows it (Table 3.1 @1M
  scan 5.6M → **35.7M rec/s** with no env). Read-only → crash **29**, no format
  bump, no §3.
  - **G1 global cap:** process-wide worker budget (`GLOBAL_MAX`/`AVAILABLE`) +
    `WorkerLease` RAII admission (`acquire()` CAS-takes `min(degree, available)`,
    releases on Drop even on `?`; `<2` → serial). **Total live workers never
    exceed the cap across all concurrent queries** — no more M×N oversubscription.
    `UNIDB_PARALLEL_MAX_TOTAL_WORKERS` / `Engine::set_parallel_scan_max_total_workers`.
  - **G2 timeout/cancel:** `query_limits::snapshot_deadline()` (Send+Sync deadline
    + CancelToken); workers check every few pages → `QueryTimeout`/`QueryCancelled`.
    A runaway parallel scan is now interruptible like the serial path.
  - **G4 default-ON** (`ENABLED = true`); `UNIDB_PARALLEL_SCAN=0` /
    `set_parallel_scan(false)` remain the field revert. Tests:
    `parallel_scan_global_cap_bounds_concurrency`, `parallel_scan_honors_cancellation`.
    Full lib (373) + crash (29) green default-on. Detail: `PROGRESS.md` "Parallel
    worker governance (item 15)"; `docs/backlog/15_parallel_worker_governance.md`.
- **Milestone P follow-up — parallel filtered SELECT — SHIPPED (2026-07-11),
  branch `parallel-index-select`, PR pending.** Closes the worst remaining ÷PG
  gap: filtered `SELECT … WHERE k …` (~0.14× vs PG) routes through the B-tree
  index-candidate path (`try_exec_select_btree`), which resolved candidates
  serially (random `heap.get` + `body` decode per row) — now the candidate
  `RowId` list is partitioned across workers (`parallel_resolve_candidates`;
  `heap::get_visible` extracted so a worker resolves with a Send+Sync reader).
  **Measured: 6.41×** (500k rows, `SELECT id,body WHERE k>=250000`: 995k → 6.4M
  rec/s). Read-only; crash 29; default-off toggle. `tests/parallel_scan.rs` now
  has an index-served filtered-SELECT case. Full detail in `PROGRESS.md`'s
  "Milestone P follow-up — parallel filtered SELECT" entry.
- **Milestone P — parallel scan workers — SHIPPED (2026-07-10), branch
  `parallel-scan`, PR pending.** Partitions a table's pages across
  `std::thread::scope` workers (NOT tokio — §4) reading the shared mmap.
  **Read-only → crash harness stays 29, no `FORMAT_VERSION` bump, no §3
  decision.** Default-off toggle (`Engine::set_parallel_scan` /
  `UNIDB_PARALLEL_SCAN`) pending a soak.
  - **The Phase-B "correctness landmine" does NOT exist here** (investigated +
    resolved): unidb is **mmap-as-storage** (`Frame` = eviction metadata only;
    `write_page` writes into the mmap under its write-lock; `read_page` returns an
    **owned copy** under the read-lock), so a worker always sees current committed
    data — what `ReadHandle` (6b) already relies on. My Phase-B architect-review
    flag was a Postgres-shaped hazard that doesn't apply to DuckDB-style mmap storage.
  - **P-a** `parallel_count` (partition + sum) → B1 COUNT route. **P-b**
    `parallel_filter_project` (partition + concat, order-agnostic) → `exec_select`
    full scan + `query_exec::scan`. Config: dynamic block assignment (shared
    `AtomicUsize` page cursor, not static slices). `src/sql/parallel_scan.rs` new;
    `heap.rs` extracted `scan_page_into`/`count_page_visible`/`scan_pages`.
  - **Results (1M rows, 18 cores):** unfiltered `SELECT COUNT(*)` **3.82×**
    (77M → 295M rec/s, now ~5–8× faster than Postgres); filtered
    `COUNT(*) WHERE` **6.6×** (5.37M → 35.4M rec/s, PG lead +540% → +82%, ÷PG
    0.16× → 0.55×) via **partial aggregate** — `parallel_count_matching` +
    `QExpr::has_subquery` push the whole scan→filter→count into workers
    (subquery predicates fall back). (Base-scan-only was 1.59× before that.)
    `SUM`/`GROUP BY` partial aggregate + `LIMIT` early-stop still filed.
  - `tests/parallel_scan.rs`: parallel matches serial, honors MVCC, torn-read-free
    under a concurrent writer. Full detail: `PROGRESS.md` "Milestone P" entry;
    `docs/backlog/parallel_scan.md` (SHIPPED + follow-ups).
- **CRUD performance — Phase B (read path) — SHIPPED (2026-07-10), branch
  `crud-perf-phaseB`, PR pending.** Read-path decode-pushdown; **read-only, no
  write/recovery/format change → crash harness stays 29, no `FORMAT_VERSION`
  bump.** Reviewed under a senior-DB-architect lens before implementation
  (ordered by real ROI; parallel scan split into its own milestone).
  - **B2 (projection/qual decode pushdown) LEADS** — `decode_row` refactored into
    `decode_value_at` + `skip_value_at`; new `deform_row(bytes, cols, upto,
    needed)` materializes only referenced columns and **stops after the last
    needed index** (PG `heap_deform_tuple` `natts` limit). Two-phase decode
    (predicate cols → test → projection cols only on match) in `exec_select`,
    `exec_select_readonly`, `matching_rows`, and **`try_exec_select_btree`** (the
    SELECT-filtered hot path — a range predicate is served there, not the full
    scan). Result: SELECT filtered `dec/row 2.00 → 0.00`, `cols/row 8.00 → 5.00`,
    +28% absolute.
  - **B1 (`SELECT COUNT(*)` count-visible-slots)** — `Heap::count_visible` (header-
    only, `on_read` for SSI parity, no decode); routed in `query_exec` for
    `COUNT(*)`-only aggregates over a plain Scan. **Result: unidb 81.4M rec/s vs
    PG 29.0M — unidb 2.81× FASTER** (rare single-model win). Honest ceiling:
    O(pages) header scan, no visibility-map shortcut at large scale (filed).
  - **B5** — `index_matching_rows` sorts candidates by `(page, slot)` before
    `heap.get` (bitmap-style sequential access; softens the A3 random-access
    cliff). SELECT-path reorder + `ORDER BY…LIMIT` early-stop filed as follow-ups.
  - **Acceptance:** COUNT gap `≤2×` **exceeded** (unidb 2.81× faster); filtered
    SELECT `≥0.5×` **not met** (~0.17× / +28% absolute) — that query projects
    `body` (still materialized for matches) and PG's tight scan leads; the scan
    gap needs **parallel scan (Milestone P, `docs/backlog/parallel_scan.md`)**.
    C1′ added a `cols/row` bench column. Peak RSS 17.5 MB. `query_exec` scan
    projection is a filed follow-up (needs planner column pruning). Full detail
    in `PROGRESS.md`'s "CRUD performance — Phase B" entry.
- **CRUD performance — Phase A (write path) — SHIPPED (2026-07-10), merged to
  `main` via PR #34 (`e6fd0cb`).** Closes the Table-3 UPDATE-bulk CRUD-stress
  gap vs matched-durability Postgres 18.4. **Headline: UPDATE bulk 0.11× →
  0.34×** (3.3× faster) by collapsing index-maintenance WAL **8868 → 619 B/row
  (14×)**; DELETE selected no regression; INSERT/SELECT untouched; crash harness
  **28 → 29**. Ordered checkpoints C1 → A1 → A3 → A4 (each its own commit). **Two
  sign-offs (recorded in `PROGRESS.md`):**
  - **A1 shipped as WAL *coalescing*, NOT the plan's "skip unchanged-column
    index maintenance" — the plan's skip is provably incorrect here.** This
    engine does insert-new-version (`heap.update` → new RowId, backward-only
    chain; `heap.get` never walks forward), so the B-tree is the ONLY
    forward-resolution mechanism; skipping an entry makes the live row
    **unfindable by any index scan** (verified: a point `SELECT … WHERE k=x`
    returned `[]` after a non-key UPDATE with the write skipped). What shipped:
    `DiskBTree::insert_many` logs each dirtied leaf **once per statement**
    (per-leaf latch, re-read under latch, fallback to per-entry crabbing insert
    on split/boundary), keeping every entry. Same RC2 win, no bug. Redo-only
    `WAL_INDEX` unchanged; no `FORMAT_VERSION` bump.
  - **A2 (HOT same-page update) NOT attempted** — genuinely fiddly against the
    MVCC model (needs forward-chained heap + stable index target + reader
    forward-walk = format + recovery change; naive in-place is unsafe for
    concurrent snapshots). It is the real path to UPDATE *parity* — filed.
  - **A3 (index-driven UPDATE/DELETE via `index_matching_rows`) is
    selectivity-GATED** (`index_lookup_is_selective`): equality always uses the
    index; a range only when ANALYZE (P4.d) stats say selectivity ≤ 0.3. Measured:
    forcing the index on a 50%-selective DELETE **regressed** it (random heap
    access loses to a sequential scan when matches aren't few). The bench now
    `ANALYZE`s both engines before UPDATE/DELETE so each planner is stats-informed
    (fair + demonstrates the gate: 25% UPDATE → index dec/row 1.00, 50% DELETE →
    scan dec/row 2.00).
  - **A4** — `exec_update` computes `has_unique` once and skips the per-row
    `snapshot_for_statement` + `enforce_unique` scan when the table has no UNIQUE
    set (was allocated per row).
  - **Acceptance revised (sign-off):** the original ≥0.8× write-path target is
    architecturally unreachable in scope — after A1 removed the *removable*
    index-WAL waste, the residual is the insert-new-version MVCC cost (needs
    HOT/A2) and PG's parallel/tight-C scan+mark-delete (needs Phase-B
    decode-pushdown). Shipped the measured win; filed A2 + Phase B as the path to
    parity. **Phase B (scan/read path) not started.**
  - **C1 measurement infra:** `Engine::wal_total_bytes_appended` (cumulative WAL
    bytes, survives truncation) + `Engine::rows_decoded_total` (a `ROWS_DECODED`
    atomic in `decode_row`); `decompose.rs` Table 3 gained WAL-B/row + dec/row
    columns. New crash point **P29**. Full before/after in `PROGRESS.md`'s "CRUD
    performance — Phase A" entry; spec `docs/backlog/crud_performance.md`
    (status flipped, with an inline correction block).
- **Docker fair-fsync report + Table 3 remark & Table 3.1 bulk stress — DONE
  (2026-07-10), branch `bench-docker-fair-fsync-report` (commit `c5c150c`), PR
  raised.** **Benchmark tooling only — NO engine code touched; no
  `FORMAT_VERSION` bump, no crash point, no §3 decision.** Adds a **Docker** path
  (`docker/` + `scripts/report.sh` auto-selects Docker/native) that runs the
  unidb-vs-Postgres multi-model comparison on **Linux**, where both engines share
  plain `fsync()` — removing the macOS `F_FULLFSYNC`-vs-`fsync` asymmetry.
  **unidb runs EMBEDDED in the `decompose` bench binary inside the `bench`
  container** (it's a library, not a server — there is no separate "unidb
  container"); Postgres runs in its own container. `decompose.rs` `mmreport`
  gained: **Table 3** a winner·margin **remark** column (+ INSERT row relabelled
  "per-row commit" with a per-fsync-floor note); **Table 3.1** a new bulk-stress
  section — fresh-table load + full **heap** scan (`COUNT(*) WHERE body <> 'x'`, a
  non-indexed predicate so neither engine serves it index-only), swept 10k→2M by
  default (`MM_BULK_SIZES`; 5M/10M opt-in — engine verified to ≥5M, ~2.7 min
  insert/engine, flat ~30k rec/s, no `HeapFull`). Also: `unidb-server` default
  `UNIDB_DATA_DIR`→`/tmp/unidb`; `mm_resource_report.py` correlates docker-stats
  to phase windows; `GIT_BRANCH` now passed through compose (header was `?`).
  **Two honest asymmetries stated in-report:** (1) Docker-Desktop-for-mac VM
  `fsync` is not flush-to-platter → PG per-commit is artificially cheap, ratio is
  fair but absolute durability is VM-bound (run on native Linux for publishable
  numbers); (2) Table 3.1 scan lead at scale = PG **parallel** seq-scan vs unidb
  **single-threaded** scan (real capability gap, not a count shortcut). Full
  numbers + before/after in `PROGRESS.md`'s "Docker fair-fsync report" entry;
  latest generated report (git-ignored) `docker/out/multi_model_report_20260710_065526.md`.
- **Index & heap write concurrency (0a + 0c + Item A) — COMPLETE (2026-07-10),
  branch `index-write-concurrency`.** Raises the concurrent **indexed** SQL-write
  ceiling, behind a **default-off `UNIDB_CONCURRENT_SQL_WRITES` toggle**
  (`AtomicBool`; `Engine::set_concurrent_sql_writes` flips it at runtime — the
  revert-in-the-field safety net). Spec/DoD: `docs/backlog/index_write_concurrency.md`
  (flipped to SHIPPED). **The first landed unit is exactly 0a + 0c + Item A; 0b
  (per-table lock registry) and Item B (heap-tail spread) are deferred/unlanded.**
  - **0a** — `ExecCtx.catalog` is now `CatalogHandle{Shared(&Catalog),
    Exclusive(&mut Catalog)}` (Deref for reads; `.exclusive()?` for the 8 catalog-
    write sites — a `Shared` handle erroring there is a routing tripwire).
    `Engine::execute_one_plan`/`stmt_uses_shared_catalog` route catalog-non-mutating
    DML (SELECT/INSERT/UPDATE/DELETE on an FSM-backed, non-SERIAL table) → `cat_read`;
    DDL + catalog-mutating DML → `cat_write`. **Toggle off ⇒ everything is
    `cat_write`, byte-for-byte the old behavior** (all default + server + crash
    tests green with the toggle off).
  - **0c** — INSERT into a SERIAL/identity table, or any DML on a legacy pre-FSM
    (`fsm_meta==None`) table, *escalates* to the exclusive path (those mutate the
    catalog). The SQL DML path already did NOT take `write_serial` (audited); graph/
    LOB/event keep it (out of scope). Atomic-counter/batched SERIAL is a filed
    follow-up (not needed — acceptance table has no SERIAL).
  - **Item A** — `DiskBTree` writes are race-safe under concurrent writers via
    **latch-coupled ("crabbing") descent with safe-node early release** (`insert_in_txn`
    rewritten iterative; recursive `insert_into` removed). Latch each child before the
    parent over the P5.a per-page exclusive latches; drop all ancestor+meta latches at
    the first `node_is_insert_safe` node (exact for Int/Bool keys, conservative for
    Text); the `retained` frame-stack suffix stays latched; only a root split repoints
    the meta page (root never released ⇒ meta held). Latches strictly root→leaf ⇒
    deadlock-free. `set_value`/`remove` **re-read the leaf under its exclusive latch**
    (never clobber a concurrent split with pre-latch bytes). Reads stay latch-free
    (owned per-page copies + right-linked leaves + MVCC re-validation self-correct).
    Recovery unchanged (redo-only `WAL_INDEX`, one mini-txn/insert) → **crash harness
    still 28/28**. No `FORMAT_VERSION` bump.
  - **Validation:** `DiskBTree::validate` structural validator; `btree_index`
    concurrent-stress (8×500, disjoint+overlap) + deterministic split-contention
    tests; `tests/concurrent_writers.rs` end-to-end indexed 8-writer (toggle on AND
    off), vacuum-interleaved (M10.c aliasing), 2-thread deadlock-no-hang;
    `TableDef.generation` tripwire (DDL bumps, DML `debug_assert`s stable); **`loom`
    model** in isolated `loom-crabbing` crate (`RUSTFLAGS="--cfg loom" cargo test -p
    loom-crabbing` — kept separate so `--cfg loom` never reaches tokio/postgres
    dev-deps). TSan is the documented CI hook (Linux; dev machine is Apple silicon).
  - **Acceptance (Table C, `UNIDB_BENCH=hiconc HICONC_ONLY=c`, 200k pregrow,
    native):** indexed 8-writer **768 (off) → 1058 (on) commits/s (+38%)**, toward the
    ~1260 unindexed floor; unindexed unchanged (fsync-bound); **toggle-off reproduces
    768.** Residual gap = `WAL_INDEX` full-page-image append contention (WAL-format-
    inherent), not tree latching. (Spec's `904 → ~1290` was a different machine; same
    mechanism/direction.) Full before/after: `PROGRESS.md` "Index & heap write
    concurrency" entry; `engine_design.md` §5.4 updated; `README.md` Phase-5 line
    updated; `high_scale_concurrency.md` Table C post-fix note added.
  - **Follow-up:** a later commit flips the toggle **default-on** after a soak,
    recorded in `PROGRESS.md`. Optimistic shared-latch descent + full Lehman-Yao
    B-link (format-bump-gated) to overlap same-subtree descents; 0b; Item B.
- **Coordinator housekeeping (2026-07-10) — `main` fully green.** `GET /tables`
  merged (PR #28); studio-UI spec closed as not-needed (PR #27); **build hotfix**:
  registered `tests/server_tables.rs` behind `required-features = ["server"]` in
  `Cargo.toml` — #28 left it unregistered, so the default `cargo test -p unidb`
  (no server feature) auto-discovered it and failed to compile (the `--features
  server` CI stayed green, which is how #28 merged). Verified: crash harness
  **28/28**, clippy/fmt clean, 0 async-deps, default + server suites pass.
  Worktrees `../unidb-fsm` and `../unidb-tables` removed; `../unidb-pgbench` kept.
- **Durable on-disk FSM + catalog page-list — COMPLETE (2026-07-10), merged to
  `main` via PR #29 (ordered commits B1 → B2 → B-accept + docs).** Closes the
  SQL-path `HeapFull{8138}` scaling ceiling the Postgres baseline (PR #25)
  root-caused, and the §12 "durable on-disk FSM fork" tech-debt item. **Root
  cause:** `TableDef.pages: Vec<PageId>` lived inline in the single JSON catalog
  blob, and the SQL insert path rewrote the whole list into it on every heap-page
  alloc (`persist_pages_if_changed` → `set_pages`); at ~900–1,450 pages the blob
  overflowed one 8 KiB page → next INSERT failed. **Fix:** the page directory +
  free-space map become a per-table durable `DiskBTree` keyed `page_id →
  free_bytes` (keys = the directory), meta page id in `TableDef.fsm_meta`
  (`#[serde(default)]`; `pages` kept as legacy fallback — **no data-dir
  migration, no `FORMAT_VERSION` bump**). WAL-logged + crash-recovered by
  inheritance (`WAL_INDEX`); `Engine::open` stays O(1). **B1** (`c6bb225`):
  directory off the blob — `DiskBTree::max_entry` (O(log n) append tail) +
  `page_directory` (leaf walk over any `PageReader` — pool *or* concurrent-read
  mmap); `Heap::open` O(1); `persist_pages_if_changed`/`set_pages` no-ops for
  FSM-backed tables (`Heap::is_fsm_backed`); all ~24 `from_pages` sites →
  `Heap::open`; the legacy raw-CRUD `self.heap` is unchanged (no fsm_meta).
  **B2** (`4f4a69c`): free-space durable (value's slot = free bytes;
  `ensure_directory` warms the free map on reopen — no cold re-probe);
  `DiskBTree::insert_in_txn` makes the heap grow atomic (page init + FSM entry in
  ONE mini-txn → **no orphan on crash mid-grow**); `DiskBTree::set_value`
  (in-place, no split) lets vacuum `compact_page` persist reclaimed free durably
  (autovacuum integration; P26 still green). **Throughput guard:** the hot
  per-row insert path does NOT write the tree (a full-page-image `WAL_INDEX` per
  row would bloat the WAL) — free-space persisted at alloc + vacuum only.
  **Crash harness 26 → 28** (P27 durable FSM directory survives a no-checkpoint
  crash + reopened heap appends at the recovered tail; P28 atomic grow leaves no
  orphan). **B-accept** (`benches/decompose.rs`, `UNIDB_BENCH=fsm`/`b3`, native
  M5 Pro, vs `main` `ecd2f1e`): **(1) correctness PASS** — before dies at ~876
  pages (`HeapFull 8141`), after builds clean to ≥2,000 pages; **(2) insert cost
  at scale** — before rises 65→108→173 µs/row (O(pages) blob rewrite) then
  errors, after flat ~17–28 µs/row (~6.5× faster at 750 pages); **(3) concurrent
  SQL writes (the requested refinement) — NO measurable improvement** (before/
  after B3 indistinguishable ~1150–1230 commits/s @ 8 writers): the microbench
  table is ~40 pages so `set_pages` rarely fired; the bottleneck is group-commit
  fsync + the per-statement catalog `RwLock`, unchanged — the `set_pages` win
  only bites at large table sizes (the (2) numbers). Full detail +
  before/after tables in `PROGRESS.md`'s "Durable on-disk FSM" entry;
  spec/status in `docs/backlog/durable_fsm_catalog_pagelist.md`.
- **Autovacuum — COMPLETE (2026-07-09), on branch `autovacuum` (one PR,
  checkpoints A1–A4 as ordered commits).** Closes the one automation gap the
  Postgres baseline surfaced: M10 `Engine::vacuum` was manual-only, so sustained
  churn bloated reads. A background **`std::thread`** launcher (NOT tokio — §4
  sync-core invariant held; `cargo tree` free of tokio/reqwest/axum) now
  **auto-triggers that same, already-safe M10 vacuum** on a Postgres-shape policy
  `dead > threshold + scale_factor·live`. **No reclamation re-implemented; the
  vacuum horizon is untouched** (reader-correct P5.c, slot-pinned P6.b) —
  autovacuum only decides *when*. Checkpoints: **A1** global `dead_tuples`/
  `live_tuples` atomic estimates (Postgres `n_dead_tup`/`reltuples`-style),
  counted at the raw-CRUD + SQL-statement chokepoints (never in `heap.rs` —
  recovery redo drives that), refreshed by `vacuum_inner`; **A2**
  `AutoVacuumConfig{enabled,threshold,scale_factor,naptime}` mirroring
  `AutoCheckpointConfig`, env knobs `UNIDB_AUTOVACUUM_{ENABLED,THRESHOLD,
  SCALE_FACTOR,NAPTIME_SECS}`, default-on (50/0.2/60 s), pure `should_vacuum`;
  **A3** `src/autovacuum.rs` — the worker holds a **`Weak<Engine>`** (a strong
  `Arc` would form a refcount cycle preventing `Engine::Drop`), the
  `AutoVacuumHandle` is an engine field so field-drop = clean shutdown (M2.b-style
  bounded join + a `worker_id` self-join guard for the external-drop-mid-pass
  race); `spawn_autovacuum(&Arc<Engine>)` + `open_arc()` (default-on, wired into
  the server); a bare `Engine::open` is thread-free by construction
  (deterministic tests; manual `vacuum()` always available); **A4** stats via
  `EngineStats`/`/stats`/`/metrics` gauges, `run_autovacuum_pass` public. **Why
  concurrent background vacuum needs no new locking (M3.b-style):** `Engine` is
  `Send+Sync` (P5.e), `vacuum` already takes `write_serial` + per-page latches
  (M10) so a background pass interleaves exactly as a *manual* `vacuum()` already
  does; `WAL_VACUUM` is redo-only/idempotent (P10) so crash-during-autovacuum
  recovers identically. **Crash harness 25 → 26** (P26: crash after an autovacuum
  pass through a real SQL table + durable BTREE index — reopen, live row survives,
  reclaimed stays reclaimed, re-vacuum idempotent). **Benchmark** (`benches/
  vacuum.rs`, logical heap pages since physical file is quantized to P1.c's 4 MiB
  chunks): 200×30 churn → **82 pages un-vacuumed vs 35 with background autovacuum
  (2.3× fewer, bounded)** vs 17 manual-every-round. Known limits (documented):
  global (not per-table) estimates + whole-engine pass (per-table `vacuum_table`
  + cost throttle are the follow-up); estimates approximate (drift until vacuum
  refresh); a horizon-holding RR reader/slot makes it re-run reclaiming nothing
  until it advances. No `FORMAT_VERSION` bump; no §3 decision touched. Full detail
  in `PROGRESS.md`'s "Autovacuum" entry + `docs/backlog/autovacuum.md` (status
  flipped to SHIPPED).
- **Postgres baseline comparison — COMPLETE (2026-07-09), on branch `pg-baseline`
  (one PR, checkpoints B1–B4 as ordered commits).** A **fitness check** — unidb vs
  PostgreSQL 18.4, both as shipped, CRUD-only overlap — distinct from the ladder
  (PR #24) and the future replaced-stack headline. **Benches + script + docs only;
  no engine code touched.** `benches/decompose.rs` gained `PG_URL`-gated configs
  (`postgres` **dev-dep only**, sync invariant verified clean) that flip Postgres's
  server-wide `wal_sync_method` via `ALTER SYSTEM`+`pg_reload_conf()` and **report
  two durability lenses side by side, never one alone** (the spec's core rule):
  lens 1 = `open_datasync` (macOS PG default, not flush-to-platter), lens 2 =
  `fsync_writethrough` (F_FULLFSYNC, matches unidb). Every printed number is
  labelled with the sync method actually in force (verified via `SHOW`).
  `scripts/pg_compare.sh` does native-preferred bring-up (Docker mode prints the
  VM-durability caveat), both lenses, teardown, peak-RSS capture. **Environment:
  NATIVE macOS 26.4, Apple M5 Pro (18 cores), PG 18.4 Homebrew, local Unix socket.**
  **Headline (lens 2, matched durability):** durable insert **parity** (unidb
  3.58 ms vs PG 3.31 ms/row); point reads **unidb ~4.9× faster** (6.87 µs embedded
  vs 33.6 µs); concurrent writes **scale on BOTH unidb raw AND SQL paths** (3.55×/
  3.82× at 8 cores, matching PG's 3.81×) — **refuting filed prediction 3** (the
  catalog-`RwLock` serializes only the fast in-memory work; group commit coalesces
  the dominant fsync outside the lock); size sweep **flat 10k→1M** (nothing bends,
  unidb read ~13× faster at every size). The one honest gap: 30× update churn
  bloats unidb reads (6.8→35 µs) with no autovacuum, but a manual M10 `vacuum()`
  restores 5.85 µs (better than fresh) — automation gap, not capability. Peak RSS
  ~35 MB. B4 unidb uses the **raw** path (P1.c claim); the SQL bulk-load path hits
  `HeapFull` at ~145k rows. **Root cause (corrected — not the "lazy FSM" the
  first pass claimed):** the catalog is a single JSON blob and `TableDef.pages`
  is an unbounded `Vec<PageId>` (one per heap page); the SQL insert rewrites it
  into the blob on every page alloc, and at ~1,450 pages the blob overflows a
  single 8 KiB page → `HeapFull{size:8138}` (the blob, not a data row). Raw insert
  never rewrites the catalog → immune (builds 5M linearly). An **O(heap-pages)
  catalog-size cap**, fix specced in `docs/backlog/durable_fsm_catalog_pagelist.md`.
  Predictions-vs-actuals table + verdict in
  `PROGRESS.md`'s "Postgres baseline comparison" entry. Linux re-run is the filed
  follow-up for publishable numbers. No `FORMAT_VERSION` bump; no §3 decision touched.
- **Commit-time WAL fsync — COMPLETE (2026-07-09), on branch `commit-time-fsync`
  (one PR, checkpoints C1–C5 as ordered commits).** Flipped the durability
  default to **group-committed force-log-at-commit**: statement mini-txns issued
  inside an open user transaction append their WAL records without a
  per-statement fsync; `Engine::commit`'s `sync_up_to` is the single durable
  point (one group-coalesced fsync per transaction). ARIES force-log-at-commit —
  **fulfills D1; D2 (mini-txn bracketing) and D5 (WAL-before-page) unchanged, no
  §3 decision reversed.** Human sign-off for making it the default recorded in
  `PROGRESS.md` (2026-07-09). Checkpoints: **C1** `Engine::open` sets deferred by
  default (after open-time system setup, which stays per-statement-durable);
  `set_deferred_sync` is now `#[doc(hidden)]` (legacy per-statement policy kept
  only for the harness); standalone durability-claim sites self-sync (checkpoint
  `wal.sync()` before `flush_all`, vacuum, `set_column_index`, `enable_events`) —
  full audit table in `PROGRESS.md`. **C2** eviction-forced sync
  (`fetch_page_for_write` already forced `wal.sync()` + retry; added the
  memory-pressure test) — which **surfaced + fixed two pre-existing latent
  recovery bugs**: (i) WAL_INSERT redo leaked a buffer-pool frame pin on its two
  early-return paths (alloc record `slot==u16::MAX`, and the idempotent skip),
  exhausting a small recovery pool; (ii) recovery replayed with
  `durable_wal_lsn=INVALID_LSN`, so `find_victim` couldn't evict any dirty redo
  page — both only bite when recovered data spans more pages than the recovery
  pool (normal 4096-frame recovery never hit them). **C3** WAL shipping
  (`records_from`/`ship_from`) capped at the durable frontier (divergence guard —
  a replica stays a prefix of the primary on failover); new
  `Engine::wal_durable_lsn()`. **C4** crash harness **21 → 25** (Pa mid-txn
  unsynced → zero trace, Pb cross-txn shared-log sync cleanly undoes the open
  txn, Pc torn unsynced tail, Pd eviction-forced-sync D5 ordering) + the
  valid-prefix property test now runs under **both** policies. **C5** acceptance
  bench (`benches/decompose.rs`, fetched from `origin/bench-ladder`): the
  ordinary rungs now converge with the explicit one-fsync rungs (proof the flip
  landed) — full multi-model commit **~33.1 → ~4.40 ms/commit (~7.5×)**, W0 at
  SQLite parity (3.59 vs 3.64 ms). **No `FORMAT_VERSION` bump; sync invariant
  holds.** Async derivation stays parked (re-trigger = re-run the ladder at large
  table sizes). Full detail + before/after table in `PROGRESS.md`'s "Commit-time
  WAL fsync" entry.
- **Phase 6 (Operations & HA) — COMPLETE (2026-07-09), on branch `phase6-ops-ha`
  (one PR for all of P6.a–P6.g).** The roadmap's 6-phase plan is now fully
  delivered: unidb is a deployable, operable **single primary + read replicas**.
  Checkpoints (each its own commit): **P6.a** segmented WAL (`db.wal/` is a
  directory of 16 MiB segments; truncation deletes whole consumed segments) ·
  **P6.b** replication slots (`slots.json`) + WAL shipping (`ship_wal`/
  `decode_stream`, REST `/replication/*`) · **P6.c** read replicas
  (`replication::Replica`: base snapshot + incremental WAL apply) + `promote()`
  failover + `wait_for_sync_replicas` sync option · **P6.d** backups + PITR
  (`Engine::base_backup`/`archive_wal`, `backup::restore(..., target_lsn)` —
  PITR by LSN) · **P6.e** users/roles/GRANT (`authz::RoleStore` in `roles.json`,
  `execute_sql_as` enforcement, per-user JWT `sub`, open/bootstrap mode) ·
  **P6.f** security: native TLS (rustls/`axum-server`) + audit log
  (`audit.log`) — **encryption-at-rest DEFERRED, D9 sign-off-gated** (mmap page
  store + on-disk format) · **P6.g** observability: `Engine::stats()` +
  `GET /stats` + slow-query log + `docs/ops_runbook.md`. **Crash harness 19 →
  21** (P18 segmented-WAL, P19 backup+PITR-restore). Sign-offs recorded in
  `PROGRESS.md`: **D6** evolved (segmented WAL) + **§1** "no cloud control plane"
  relaxed for ops (both 2026-07-09). No `FORMAT_VERSION` bump; sync invariant
  holds (no tokio/reqwest/axum/rustls in the default build). Benchmark table +
  full detail in `PROGRESS.md`'s Phase 6 entry. **Key documented limitations:**
  incremental replica/PITR roll-forward reconstructs pages present in the base
  (fresh pages aren't FPI-covered — re-base regularly); PITR is by-LSN;
  RLS-over-SQL, encryption-at-rest (D9), and an auto-failover coordinator are
  follow-ups.
- **Phase 5 (concurrency & performance) — COMPLETE (2026-07-09).** Part 1
  (P5.a–P5.d) shipped to `main` via PR #14 (merge `30109d9`); Part 2 (P5.e
  multiple writers + P5.f resource control) shipped on branch
  `p5e-concurrent-writers`, **merged to `main` via PR #16** (merge `12ca9f9`). **`Engine` is now `Send +
  Sync`, a worker pool shares `Arc<Engine>`, heap page latches + leader-election
  group commit make write throughput scale with cores (3.68× at 8 writers), and
  per-query timeouts/cancellation/`work_mem` are in place.** Crash harness 19/19;
  sync invariant holds. Docs closeout done (README, `docs/design/engine_design.md`,
  `PROGRESS.md` Phase 5 entry, `docs/backlog/phase5_concurrency.md`). **PR-history
  note:** the harness auto-created+merged **PR #15 at an early `wip(P5.e-2)`
  snapshot (`7e4b89b`)** — it does *not* contain the finished work; **PR #16 (from
  the same branch → `main`) is the real, complete Phase 5 pt.2**, merged as
  `12ca9f9`; PR #15 now carries a comment pointing here. Detail below.
  - **What shipped (concurrency infrastructure — non-breaking; single-writer
    behavior is unchanged, these just make the internal components
    concurrency-capable):** P5.a concurrent buffer-pool latching (`Mutex<PoolState>`
    frames, mmap behind `Arc<RwLock>`, hand-rolled `unsafe`-free per-page S/X
    latch table; D5 preserved); P5.b concurrent WAL append (`Mutex<WalInner>`,
    `&self`, serialized LSN + group-batched flush); P5.c concurrent txn manager
    (`&self` `LockManager`, txn write path takes `&Wal`/`&LockManager`, +3
    concurrency stress tests); P5.d real lock manager (shared/exclusive modes,
    blocking `Condvar` wait queues, **wait-for-graph deadlock detection** →
    `DbError::Deadlock` → 409; SI first-committer-wins kept as the `NoWait`
    policy; +4 multi-threaded tests incl. a genuine 2-thread deadlock). Crash
    harness still **19/19**; sync-invariant holds (no tokio/reqwest/axum in the
    default engine). **Every storage component EXCEPT `Heap` is now `&self`.**
  - **P5.e step 1 — `Heap` → interior-mutable `&self` — DONE** (branch
    `p5e-concurrent-writers`, commit `75eaaa1`; green: crash harness 19/19,
    clippy/fmt/sync-invariant clean). Free-space map + page list now live behind
    a `Mutex<HeapFsm>`; **critical invariant** — that lock is *never* held across
    a page-latch acquisition or WAL I/O, so no lock-ordering cycle with the P5.a
    per-page latches (`find_or_alloc_page` probes with the lock released;
    `note_free_space` records the free *value* captured after unpin;
    `alloc_heap_page` does all page I/O before taking the lock). `page_ids()`
    now returns an owned `Vec`; `txn::abort` now takes `&Heap`/`&BufferPool`.
    **Every storage component is now `&self`/shareable — the `Sync` Engine
    foundation is complete.**
  - **P5.e steps 2–4 — DONE** (branch `p5e-concurrent-writers`, 2026-07-09):
    - **Step 2 (`0478db7`) — `Engine` is `Send + Sync`.** 6 mutated fields →
      interior-mutable (`control → Mutex<ControlData>` + cached immutable
      `page_size`; `next_lob_id`/`next_event_seq`/`checkpoints_triggered` →
      atomics; `auto_checkpoint`/`last_checkpoint` → `Mutex`); all 27 `&mut self`
      methods → `&self`; every vestigial `&mut BufferPool/Wal/…` sig+reborrow →
      `&` (those components were already `&self`). `checkpoint::run` takes
      `&Mutex<ControlData>`, locks only for the small control update (never
      across an fsync). Compile assert `Send` → `Send + Sync`.
    - **Step 3 (`f977fb3`) — concurrent writers.** `server/engine_handle.rs`
      rewritten to `Arc<Engine>` + `spawn_blocking` (channel/`worker_loop`
      deleted; read fast-path kept). **Heap page latches** (`BufferPool::
      latch_exclusive`, built in P5.a, finally wired) wrap every heap RMW → no
      lost updates; insert/update via re-checking `acquire_page_for_insert`; one
      latch at a time (no two-latch deadlock); FSM lock never nests under a latch.
      Coarse `write_serial: Mutex<()>` serializes the non-CRUD paths that do a
      non-atomic read-catalog-then-mutate-shared-index sequence (edges/LOBs/event
      tables/DDL/vacuum); **raw CRUD + reads stay concurrent**; SQL already
      serializes on the catalog `RwLock`. `tests/concurrent_writers.rs` (insert
      stress / distinct-row updates / same-row contention, deadline-guarded).
    - **Step 4 (`29fe805`) — group commit that scales.** `txn::commit` returns the
      commit LSN; `Engine::commit` forces durability via new `Wal::sync_up_to`
      (leader-election barrier); crucially `Wal::group_fsync` runs `sync_all`
      **with the append lock released** so concurrent committers coalesce behind
      one fsync. **Headline** (`benches/concurrent_writers.rs`, 8 cores): 1→325,
      2→330, 4→647 (1.99x), **8→1197 commits/s (3.68x)** — scales with cores.
      Crash harness **still 19/19** (incl. P12 fsync-fault); sync-invariant holds.
  - **P5.f — DONE** (`6f8e8c4`): query timeouts, cancellation, per-query
    `work_mem` — a thread-local `QueryLimits` via an RAII guard; scan loops check
    every 1024 rows (`DbError::QueryTimeout`/`QueryCancelled`), sort/hash-join
    spills consult `work_mem_rows`. Docs closeout done (README,
    `docs/design/engine_design.md`, `PROGRESS.md` Phase 5 entry, phase5 spec).
    Human sign-off to reverse the single-writer design is recorded in
    `PROGRESS.md` (2026-07-09). **Known limitation (documented):** only *raw
    CRUD* scales with cores; SQL/graph/LOB writes serialize (catalog RwLock /
    `write_serial`) — finer-grained (latch-coupled B-tree) index concurrency is
    future work.
- **Milestone: M0-M8 are ALL DONE.** Every milestone on CLAUDE.md's
  original roadmap (M0-M5) shipped; M6-M8 are a user-approved follow-on
  set (B-Tree indexing, CSR graph, an "attach" Rust client over REST)
  prompted by a comparison against a competing project (FFS/ffsdb). M6
  (B-Tree secondary index), M7 (CSR graph index), and M8 (attach client)
  are all closed out. The approved plan lived at
  `/Users/sagarmahamuni/.claude/plans/misty-hugging-brook.md` (M6/M7/M8
  plan, approved 2026-07-07); the still-parked Phase 2 SQL capability plan
  (OR/ORDER BY/LIMIT/aggregates/JOIN) is durably saved at `docs/backlog/
  phase2_sql_capability_expansion.md`, explicitly sequenced *behind* M8 —
  it is the standing next item if this project continues.
- **M8 was developed in a separate git worktree in parallel with M6/M7
  landing on `main`, then merged after independent re-verification — that
  re-verification is what found a real M7 bug**, not something M8 broke.
  See the corrected M7 design note below and `PROGRESS.md`'s M7 entry
  (which carries a correction block, not a rewrite of history): M7
  originally wired `edges_from`/Cypher to prefer the CSR graph index once
  `IndexStatus::Ready`, but `Ready` only means "initial backfill done," not
  "every write since is reflected in the debounced rebuild" — a
  transaction's own just-created edge could fail to appear in its own
  traversal, breaking a self-visibility guarantee M3 shipped with. Fixed
  by reverting `edges_from`/`execute_cypher` to consult `EdgeIndex`
  unconditionally, exactly as before M7. `CsrIndex` itself (construction,
  debounced rebuild, being kept warm by every live edge write) is
  untouched and still correct — only the "prefer it for traversal" wiring
  was removed. The bug reliably reproduced via `cargo test -p unidb --test
  graph_mvcc` run repeatedly *outside* the full workspace test suite, but
  was invisible in `cargo test --workspace` — worth remembering that
  workspace-level feature unification can change test binary composition/
  timing enough to mask a real, deterministically-reproducible race.
- **Critical fix landed mid-M5 (2026-07-06), its own commit, not part of
  M5's feature work:** a real xid-reuse-after-checkpoint bug was found by
  manually smoke-testing the new REST server (commit several transactions,
  `checkpoint()`, reopen — the xid counter incorrectly reset to 1). Root
  cause and fix are in the design note below and `PROGRESS.md`'s dedicated
  entry; control file format bumped v2->v3 (D3/D9), human sign-off
  confirmed with the user before implementing. This predates M5 entirely
  (an M1-era gap) but was only surfaced by M5's checkpoint+reopen usage
  pattern, which no prior test exercised.
- **M5.d benchmark headline (full table in `PROGRESS.md`'s M5 entry)**:
  the HTTP/writer-thread layer adds only ~6% overhead over a direct
  `Engine::insert` call, and concurrent `POST /sql` throughput is *flat*
  (~135 -> ~157 -> ~158 ops/s) across 1/10/50 concurrent clients — the
  single-writer-thread design's real throughput ceiling, made concrete
  rather than assumed, and landing in the same range M1's own
  `benches/load.rs` already found for single-table INSERT.
- **State:** repo root is now a Cargo workspace (`unidb` + `unidb-attach`
  members). `cargo test -p unidb` (default features): 225 unit tests + 11
  crash-harness + 4 `graph_locking` + 3 `graph_rebuild` + 2 `graph_mvcc`
  (both back to their pre-M7 counts after the CSR-path tests were added
  and then removed during the M7 correction — no coverage was lost, since
  the underlying `EdgeIndex` path was already covered by these) + 5
  `index_rebuild` + 1 `vector_mvcc` + 1 `btree_mvcc` + 4 `queue_vacuum` +
  2 `queue_mvcc` = 258 total. `cargo test -p unidb --features server` adds
  25 `server_*` integration tests plus 3 more feature-gated unit tests
  (228 unit, matching the `--workspace` run's unidb portion). `cargo test
  --workspace` also runs `unidb-attach`'s 19 integration tests (3 CRUD + 6
  extras + 4 graph + 6 SQL) + 1 doctest, all green. `cargo clippy
  --workspace --all-targets -- -D warnings` and `cargo fmt --all --check`
  clean. `cargo tree -p unidb --no-default-features --edges normal`
  confirmed empty of tokio/axum/jsonwebtoken/reqwest (the "engine stays
  sync" claim holds for the default build's actual library/binary
  artifact even inside a workspace containing a crate that *does* depend
  on `reqwest` — note `-p unidb --edges normal` is required: plain `cargo
  tree --no-default-features` from a workspace root shows the whole
  workspace's dependency union, which is *not* the right check here).
- **Current work (Core lane): Phase 3 — Multi-model durable storage — COMPLETE**
  (the moat, `docs/backlog/phase3_durable_storage.md`). **P3.c PRODUCTION shipped
  on branch `p3c-vector-production` (2026-07-09)** — the spike's `DiskIvfIndex` is
  now the live vector index, the async index worker is retired, and **`Engine::open`
  does ZERO index rebuilding for every index type (the O(1)-open moat is real).**
  Crash harness 18 → **19** (P17). See the P3.c-production subsection below. Prior
  Phase 3 work (branch `durable-storage`): **P3.a
  + P3.b are SHIPPED.** All three key→postings secondary indexes are now durable
  on-disk `DiskBTree`s (node pages in the shared page store, WAL-logged as full
  node-page images via the redo-only `WAL_INDEX`, crash-recovered, **not rebuilt
  on open**):
  - **P3.a** — the M6 B-Tree (`DiskBTree`, stable meta page id in
    `ColumnDef.index_root`, moved off the async worker). `FORMAT_VERSION` **4→5**.
  - **P3.b** — **full-text** (inverted; keys on tokens, new read path
    `Engine::search_fulltext`) and the **edge-adjacency index** (`__edges__.
    from_id` as a durable BTree, `edge_index_meta` cached on the Engine) become
    durable too, reusing P3.a's machinery (**no new format version**). Removed
    `rebuild_edge_index` + the full-text rebuild. **CSR retired** (consulted by
    no read path since the M7 revert; adjacency now served durably by the edge
    index) — `rebuild_csr_index` + warm-keeping gone; `csr_index.rs` module kept
    only for its benchmark. The async index worker (at P3.b) served only the
    vector (Hnsw) index — **retired entirely in P3.c-production**.
  Crash harness **14 → 17** (P13 B-Tree total-data-loss recovery, P14 durable
  full-text, P15 durable edge index). **P3.c (on-disk vector) SPIKE is COMPLETE
  and PRODUCTION is SHIPPED** (see subsection): chose on-disk **IVF-Flat** (cell
  posting lists = a durable `DiskBTree`, centroids in a WAL-logged meta page),
  recall@10 = **1.000** matching the HNSW baseline (`src/disk_vector.rs`,
  `benches/vector_recall.rs`, `docs/design/p3c_vector_spike.md`). The spike also
  **found + fixed a real `DiskBTree` duplicate-key-spanning-leaves bug** affecting
  P3.a/P3.b (see subsection). **P3.d (large objects) is SHIPPED:** values stored
  **out-of-line, chunked (~7 KiB), and streamed** as ordinary MVCC/WAL chunk rows
  in a `__lobs__` system table indexed by a durable `DiskBTree` on `lob_id` —
  atomic with the txn, crash-recovered (crash point **P16**), vacuum-reclaimable,
  and streamed one chunk at a time (multi-GB without OOM). `Engine::put/read/
  delete_large_object` (`src/large_object.rs`). Crash harness **14 → 18**, then
  **→ 19** at P3.c-production (P17). Dated subsections below; full entries in
  `PROGRESS.md`. **Phase 3 is COMPLETE.**
- **Prior Core-lane work: Phase 1 — ACID & storage foundation** (the feature-freeze
  gate, `docs/backlog/phase1_acid_hardening.md`), on Core lane branch
  `acid-hardening`. **Phase 1 is COMPLETE — all five checkpoints shipped:** P1.a
  (full-page-writes), P1.b (fsync-failure handling), P1.c (alloc_page remap +
  configurable pool + real FSM), P1.d (isolation correctness — RC re-eval +
  SSI), and P1.e (auto-checkpoint). The feature-freeze gate is closed; next per
  `docs/backlog/roadmap.md` is Phase 2/3/4. See the
  Phase 1 section below. The roadmap is now `docs/backlog/roadmap.md` (6-phase
  plan); the older per-milestone backlog docs were retired. A CSR-preferring
  traversal fix (staleness/generation marker design) remains documented tech
  debt below — deliberately not attempted as part of the M7 bug fix.
- **SQL lane: Phase 2 AND Phase 4 are both COMPLETE — the SQL-lane roadmap
  items are fully delivered.** Phase 2 (P2.a–P2.e, branch `sql-types`,
  2026-07-08): DECIMAL+TIMESTAMP, FLOAT/UUID/BYTEA/DATE/TIME, ALTER/DROP/TRUNCATE
  + request-level DDL rollback, SERIAL, prepared statements + bind params
  (injection surface closed). **Phase 4 (P4.a–P4.e, branch `query-power`,
  2026-07-09): query power** — P4.a joins (hash + Grace spill / sort-merge /
  index-nested-loop over the durable B-Tree), P4.b aggregates + GROUP BY/HAVING
  + ORDER BY (external merge-sort spill) + DISTINCT + LIMIT/OFFSET, P4.c
  scalar/IN/EXISTS subqueries (correlated + uncorrelated) + WITH CTEs, P4.d
  ANALYZE (durable per-table statistics, never recomputed on open) + cost-based
  optimizer (Selinger left-deep DP join order + index-vs-scan), P4.e
  EXPLAIN/EXPLAIN ANALYZE. Correctness checked differentially against SQLite
  (rusqlite dev-dep). Additive only (a trivial single-table SELECT keeps its
  fast path; richer queries route through `LogicalPlan::Query`); no
  `FORMAT_VERSION` bump, no new crash point. See the Phase 4 subsection below +
  `PROGRESS.md`'s Phase 4 entry. A CSR-preferring traversal fix
  (staleness/generation marker design) is documented as known tech debt below
  but was deliberately not attempted as part of the M7 bug fix.
- **In-flight performance work (branch `m9-group-commit`, 2026-07-08, not
  yet merged):** group commit + read-only fsync skip. The diagnosis (see
  `docs/performance/fssdb/`) was that the ~3–4 ms floor on every durable op
  is per-statement fsync (two per autocommit statement: the mini-txn commit
  *and* the user-txn commit), compounded by the server's single writer
  thread serializing everything (flat ~131→149→153 ops/s at 1/10/50
  concurrent clients). Prototype landed two of three fixes: (1) read-only
  txns skip `commit_user_txn` entirely (`txn.rs`) — point SELECT ~3.05 ms →
  **1.09 µs**; (2) a `Wal::deferred_sync` mode + the server writer thread
  batching all queued requests behind **one fsync per batch**
  (`server/engine_handle.rs`) — concurrent INSERT throughput went from flat
  to **scaling**: ~242 / ~756 / **~4,780 ops/s** at 1/10/50 clients (31× at
  50). **Item 6a also landed:** buffer-pool force-WAL-on-evict
  (`bufferpool.rs` — `durable_wal_lsn` tracking + `fetch_page_for_write`),
  making deferred mode unconditionally safe for working sets larger than the
  pool and largely fixing the pre-existing M6 `BufferPoolFull`-at-scale
  limitation. **Item 6b then landed in part on branch `m9-concurrent-reads`
  (stacked): concurrent point reads** — a `Send + Sync` `ReadHandle` lets
  `get`/`GET /rows/:id` run off the writer thread (shared `Arc<RwLock>` mmap +
  `Arc<Mutex>` txn snapshot); concurrent SQL `SELECT` is the remaining slice
  (needs shared catalog + a read-only executor path). Full plan, numbers, and
  correctness analysis are in
  `docs/backlog/group_commit_and_read_concurrency.md`. All 229 (group-commit
  branch) / 230 (concurrent-reads branch) unit + 25 server + 11 crash-harness
  + concurrency tests green; clippy/fmt clean. Default (embedded,
  non-deferred) path keeps per-statement durability; the crash harness is
  green (the new write-back-on-evict path preserves recovery).
- Two explicitly deferred follow-ups remain, neither started: (1) the
  full CLAUDE.md §6 cross-domain "replaced stack" benchmark (possible
  since all four data models + the server exist, but a separate,
  dedicated future effort per the user's confirmed decision); (2) the
  parked Phase 2 SQL capability plan (`docs/backlog/
  phase2_sql_capability_expansion.md`). See Open questions below for
  what's still unresolved from M1-M5.
- **Last updated:** 2026-07-13

### Phase 1 — ACID & storage foundation (Core lane, branch `acid-hardening`)

The **feature-freeze gate** (`docs/backlog/phase1_acid_hardening.md`): close the
silent Tier-0 correctness holes before any scale/feature work. Serial Core lane;
one PR per checkpoint (P1.a → P1.e). **In progress as of 2026-07-08.**

- **P1.a — Full-page-writes (WAL_FPI) — SHIPPED (2026-07-08).** Closes the #1
  data-loss hole: a torn 8 KiB page write (crash mid-write → half-old/half-new,
  CRC detects but can't repair). On the **first modification of a page after
  each checkpoint** the buffer pool logs the whole clean page image to the WAL
  (`WAL_FPI`, redo-only, `slot=u16::MAX`) via `BufferPool::maybe_log_fpi`,
  called from every `heap.rs` mutation right after the page fetch and before the
  incremental record; recovery (`restore_page_image`, CRC-bypassing) replays it
  as the clean base, then the interval's incremental redos (higher LSN) apply on
  top. `checkpoint::run` calls `clear_fpi_tracking()` after `flush_all` to
  re-arm the next interval. Tracking is a `HashSet<PageId>` (not a per-frame
  flag) so it survives eviction → exactly one FPI per page per interval.
  **Why sufficient:** D5 forbids flushing a page whose WAL isn't durable, so any
  torn on-disk page belongs to a *committed* mini-txn whose FPI is in the redo
  set; incomplete mini-txns never reach disk torn. `FORMAT_VERSION` 3→4 (new
  record kind, D9). New crash point **P11** (`p11_torn_page_restored_from_full_
  page_image`) manufactures a real torn page and asserts both rows recover;
  crash harness now 13 tests (P1–P11 + property). Bench `benches/fpi.rs`: FPI
  adds 12 % (8 B rows) → 47 % (1 KiB rows) WAL volume in a write-once/no-
  checkpoint worst case, **zero throughput change** (write path is fsync-bound;
  FPI adds bytes not fsyncs); auto-checkpoint (P1.e) bounds total FPI volume.
  See `PROGRESS.md`'s Phase 1 entry for the full table + the documented
  fresh-page/catalog limitation (fresh un-referenced pages aren't FPI-covered —
  no committed-data loss, tracked for a later pass).
- **P1.b — fsync-failure handling (fsyncgate) — SHIPPED (2026-07-08).** A
  failed `fsync`/`msync` may leave the OS having dropped the dirty data while
  clearing its dirty bit, so a retry can falsely succeed. Both durability
  components now latch **poisoned** on failure and return the new
  `DbError::DurabilityFailure` forever after — `Wal::fsync` doesn't advance
  `durable_lsn`; `BufferPool::flush_page` doesn't mark the frame clean and
  `flush_all` fails up-front when poisoned. Deterministic fault injection via
  `Wal::arm_fsync_fault` / `BufferPool::arm_flush_fault` (+ `is_poisoned` /
  `is_flush_poisoned`). D5 re-verified end-to-end with a new `debug_assert!`
  tripwire at the `find_victim` steal point. New crash point **P12**
  (`p12_fsync_failure_refuses_to_report_success`) injects a fault at both the
  WAL-commit and data-file-flush boundaries; crash harness now 14 tests. No
  format change. See `PROGRESS.md`'s Phase 1 → P1.b entry.
- **P1.c — alloc_page remap fix + configurable pool + real FSM — SHIPPED
  (2026-07-08).** (1) Page file grows in **4 MiB chunks** (`BufferPool::
  ensure_mapped`), remapping only on a chunk boundary — was a whole-file remap
  per page (O(N²) total). `logical_page_count` reclaims trailing all-zero slack
  on open. (2) Pool capacity configurable via `UNIDB_BUFFER_POOL_PAGES` /
  `Engine::open_with_pool_capacity`, default 256 → **4096** frames. (3) Real
  free-space map (`Heap::free_map`) replaces the linear per-insert scan — page
  selection is integer compares, not a fetch of every page; kept exact via
  `note_free_space` after insert/update/compact. Bench `benches/scale.rs`:
  `alloc_page` flat ~1M pages/s to 100k pages (was O(N²)); insert throughput
  does **not** degrade to 300k rows; point reads ~1.14M/s. No format change; D6/
  D8 unchanged. Known limit: the SQL path's per-statement `from_pages` rebuilds
  the FSM lazily (raw `Engine::insert` keeps it warm) — a durable on-disk FSM is
  a later item. **Note: page 0 is now allocatable** (the sentinel is
  `INVALID_PAGE_ID = u32::MAX`, not 0) — a fresh DB starts allocating at id 0
  instead of reserving it; no on-disk/sentinel meaning changed.
- **P1.d — isolation correctness (RC re-eval + SSI) — SHIPPED (2026-07-08).**
  (1) Write-write conflicts now classify by isolation: `SerializationFailure`
  under RR/`Serializable`, left as a no-wait `WriteConflict` under RC (where the
  fresh per-statement snapshot re-reads the tip anyway — EvalPlanQual is
  inherent to the scan-based executor; blocking-then-reeval for an *active*
  writer conflict needs a wait queue, Phase 5). (2) New
  `IsolationLevel::Serializable` + **SSI** — `SsiState` per serializable txn
  (read/write sets + in/out rw-conflict flags), `committed_ser` for concurrency,
  `ssi_note_reads`/`ssi_note_write` (called from `exec_select`/`exec_update`/
  `exec_delete`) form Cahill-style rw-antidependency edges, and `commit` aborts
  a **pivot** (in+out) with `SerializationFailure`; `Engine::commit` turns that
  into a real rollback. Reduced form: row-granularity (no predicate locks → no
  phantom protection), statement-granularity tracking at the executor (the
  `on_read`/`on_write` heap seam stays no-op for finer tracking later), and a
  write-skew pair may both abort in some orderings (sound, occasionally
  over-conservative). Tests in `lib.rs` (write-skew commits under RR / aborts
  under Serializable; RC no-spurious-abort; RR conflict→SerializationFailure;
  lone serializable commits). **No new crash point** (an SSI abort is an
  ordinary rollback — harness stays 14). No format change. See `PROGRESS.md`'s
  Phase 1 → P1.d entry.
- **P1.e — auto-checkpoint — SHIPPED (2026-07-08). Phase 1 is COMPLETE.**
  Bounds WAL growth (was manual-only → unbounded). `Engine::maybe_auto_checkpoint`
  (called from `commit`) runs the existing checkpoint path inline when a **time**
  (`checkpoint_timeout`, default 60 s) or **WAL-size** (`max_wal_size`, default
  64 MiB) trigger fires — but only at a **quiescent point** (`txn_mgr.active_
  count() == 0`), so truncation can't discard an in-flight txn's undo (a
  permanently-open long-lived txn blocks it — documented footgun). `wal.rs`
  tracks `wal_bytes` (reset on truncate); `AutoCheckpointConfig` (env
  `UNIDB_AUTO_CHECKPOINT` / `_CHECKPOINT_TIMEOUT_SECS` / `_MAX_WAL_SIZE_BYTES`),
  `set_auto_checkpoint_config` / `checkpoints_triggered`. Default-on thresholds
  are high enough not to trip existing tests. **No new crash point** (reuses the
  P2/P4-tested checkpoint path — changes *when*, not *how*; harness stays 14).
  Bench `benches/checkpoint.rs`: WAL bounded ~50 KB/154 KB vs 1.17 MB unbounded
  (~8–23× smaller), throughput unchanged (~160 rows/s). No format change. See
  `PROGRESS.md`'s Phase 1 → P1.e entry + "Phase 1 complete".

### M10 — heap vacuum / MVCC GC (Core lane, branch `core-vacuum`, 2026-07-08)

Shipped the first physical space-reclamation path in the engine — the one
place the project previously stood *in* the MVCC bloat trap rather than
sidestepping it. Built on top of the already-merged concurrent-read model
(PRs #2–#4), so the horizon includes live `ReadHandle` readers, not just the
writer's active transactions. Checkpoints M10.a→M10.d all landed; full metrics
table in `PROGRESS.md`'s M10 entry. Key points a future reader needs:

- **Horizon (M10.a).** `TransactionManager::vacuum_horizon()` = `min`
  `snapshot.xmin` over all live writer txns **and** live concurrent readers.
  Readers register via a `ReadRegistration` RAII guard returned by
  `txn::read_snapshot` and held for the whole read in `read_handle.rs` — a
  concurrent reader now genuinely holds the horizon back, closing the window
  where the writer could reclaim a version an in-flight off-thread scan still
  needs. `mvcc::is_reclaimable(xmax, horizon)` (`xmax != 0 && xmax < horizon`)
  is the deliberate inverse of `is_visible`, cross-checked against it in a
  table-driven test. A non-zero on-disk `xmax` always means a *committed*
  deleter (aborts are physically undone), which is what makes the `xmax`-only
  check sound — see the `is_reclaimable` doc comment.
- **Slot lifecycle (M10.b/d).** New `SlotState` `LIVE → DEAD → UNUSED`, encoded
  in the existing `(offset,length)` slot pair with **no format change**: DEAD =
  `(0, SLOT_DEAD_LEN=1)`, UNUSED = `(0,0)`. A real tuple length is always
  `>= 24`, so `1` can't collide. `insert_versioned` now reuses the lowest
  UNUSED slot (never a DEAD one — that's the aliasing gate). `scan`/`get`/
  `resolve_candidates_batched` skip non-live slots (this also *fixes* a
  pre-existing latent fragility: a recovery-undone incomplete-insert slot
  `(0,0)` would previously have errored a later table scan).
- **Crash-safe WAL (M10.b).** New `WAL_VACUUM` (redo-only, no undo — re-freeing
  already-dead-and-committed space is idempotent). Two shapes by `slot`:
  `slot != u16::MAX` marks one line pointer DEAD (M10.b); `slot == u16::MAX`
  carries a full compacted page image (M10.d), replayed by reconstructing the
  page and re-stamping the record LSN. Both idempotent via the page-LSN gate.
  New D7 crash point **P10** (kill mid-vacuum → reopen → committed row
  survives, reclaimed version stays reclaimed, re-running vacuum is a no-op).
- **The aliasing hazard (M10.c) — the "single most important test."** Stale
  secondary-index entries are harmless *only* while slots are never reused;
  the moment vacuum reuses a slot, a stale entry can resolve to a *live,
  MVCC-visible, semantically-wrong* row. So `Engine::vacuum` scrubs every
  reclaimed `RowId` from all secondary indexes **before** any slot becomes
  UNUSED (`EdgeIndex::remove_rowid`, `IndexHandle::remove_rows` for
  BTree/FullText/Vector). Reproduced deterministically via the `EdgeIndex`
  traversal path — `edges_from` trusts index candidates without re-checking
  `from_id`, so an aborted `create_edge` (which leaves a stale
  `from_id→RowId` entry, abort having no index hook) plus slot reuse yields a
  wrong-but-visible edge with the gate off; the real `Engine::vacuum` makes it
  impossible. `vacuum_inner(clean_indexes: bool)` exists solely so the test can
  toggle the gate.
- **Resolved plan decisions.** (1) The M10 plan assumed `VectorIndex` has no
  incremental remove; it *does* have `remove` (rebuild-based), so
  vector-indexed tables are cleaned rather than excluded from slot reuse.
  (2) `CsrIndex` is intentionally left un-scrubbed by `remove_rows`: it has no
  incremental remove, is rebuilt from committed rows on open, and — since M7's
  "prefer CSR for traversal" wiring was reverted — is consulted by no read
  path, so a stale CSR candidate can never surface.
- **Scope / known limits (documented, not silent).** ~~Manual only (no
  autovacuum)~~ — now **auto-triggered by the Autovacuum background launcher
  (A1–A4, branch `autovacuum`)**, which fires this exact pass on a threshold
  policy; long-lived RR txns / readers hold the horizon back (surfaced in
  `VacuumReport.horizon_blocked`, not swallowed); intra-page compaction only
  (no cross-page/`VACUUM FULL` high-water-mark shrink); index structures shrink
  logically (entry removal) but aren't physically rebuilt. All parked in the
  M10 plan's backlog. `Engine::vacuum()` is embedded-API only in v1 — no REST
  route (matching `vacuum_events`'s explicit-call precedent). **Cost is
  fsync-bound** (~4.3 ms per reclaimed version, benchmarked): each `mark_dead`/
  `compact_page` is its own fsyncing mini-txn, so vacuum pays ~N fsyncs for N
  versions — the same per-statement floor M1/M3 already hit. Batching them
  behind one fsync (the M9 `deferred_sync` mechanism) is the obvious future
  speedup, deliberately not done here. Leak-closed proof: under 200-key ×
  30-round update churn, periodic vacuum keeps the heap file at 73,728 B vs
  606,208 B un-vacuumed (**8.2× smaller**).

### P2.a — DECIMAL + TIMESTAMP (SQL lane, branch `sql-types`, 2026-07-08)

First checkpoint of **Phase 2** (`docs/backlog/phase2_data_model.md`), the
SQL-lane worktree that runs disjoint from the Core lane's Phase 1
(`acid-hardening`). Adds the first two "real app" scalar types — exact
fixed-point money and time — on top of the existing catalog/encoding/parser
machinery. Full entry + rationale in `PROGRESS.md`'s P2.a section. Points a
future reader needs:

- **Representations.** `ColumnType::Decimal(u8, u8)` = `(precision, scale)`;
  `ColumnType::Timestamp`. `Literal::Decimal(i128, u8)` = `(unscaled_value,
  scale)` — the value is `unscaled_value / 10^scale`, exact, never through
  `f64`. `Literal::Timestamp(i64)` = micros since the Unix epoch, UTC. Row
  encoding tags **6** (16-byte LE `i128` + 1-byte scale) and **7** (8-byte LE
  `i64`) — purely additive, **no `FORMAT_VERSION` bump** (tags only grow, old
  rows still decode per D4, a bump would needlessly reject pre-P2.a DBs and
  collide with the Core lane's Phase 1 version work).
- **The parser can't know a column is temporal** (it has no catalog), so a
  timestamp arrives as a `Literal::Text` string and is converted to
  `Timestamp` in `coerce_value`; `compare` parses a Text operand against a
  `Timestamp` on demand (`WHERE ts > '2024-01-01'`). Numeric literals with a
  fractional point parse to `Literal::Decimal` with the scale exactly as
  written (`9.90` → `(990, 2)`), rescaled to the column's declared scale at
  coercion. Narrowing scale is allowed only when dropped digits are zero
  (exact, never rounding); widening multiplies; precision cap enforced.
- **`sql/datetime.rs` is new, dependency-light** (Hinnant civil-date math, no
  `chrono`). UTC only in v1 (`TIMESTAMPTZ` normalizes to UTC, original zone
  not tracked). No `DATE`/`TIME` yet — those are P2.b and will reuse it.
- **Cross-lane compile obligation:** `queue/payload.rs::row_to_json` and
  `server/dto.rs::literal_to_json` are exhaustive `Literal` matches outside the
  SQL lane; both got additive arms rendering the new types as **strings** (so
  JSON's `f64` never truncates a decimal). Necessary to keep the default and
  `--features server` builds compiling; not a semantic change to those files.
- **M11 constraint compatibility verified end-to-end:** `DEFAULT` / `CHECK` /
  `PRIMARY KEY` / `UNIQUE` all work on both types (UNIQUE relies on
  `Literal` `PartialEq` over the coerced-to-column-scale values, so
  `2024-01-01 00:00:00` and `2024-01-01T00:00:00` collide as they should).
  No BTree index on decimal/timestamp yet (`OrderedValue` doesn't cover them;
  they're skipped in `build_indexed_columns`, not errored).

### P2.b–P2.e — rest of Phase 2 (SQL lane, branch `sql-types`, 2026-07-08)

Full entries in `PROGRESS.md`. What a future reader most needs:

- **P2.b (FLOAT/UUID/BYTEA/DATE/TIME).** Row-encoding tags **8–12** (all
  additive, no `FORMAT_VERSION` bump). `Literal::{Float(f64), Uuid([u8;16]),
  Bytea(Vec<u8>), Date(i32), Time(i64)}`. Same "parser can't know the column
  type, so uuid/bytea/date/time arrive as `Literal::Text` and coerce at
  `coerce_value`" pattern as P2.a's timestamp. `BYTEA` text = `\xHEX` or raw
  UTF-8 bytes; `UUID` in/out is canonical. Float compares via `f64` (NaN →
  unordered → false). No BTree index on these yet (`OrderedValue` skips them).
- **P2.c (ALTER/DROP/TRUNCATE + transactional DDL).** The subtle part:
  **DROP COLUMN is a logical tombstone** (`ColumnDef.dropped`), *not* a physical
  removal — removing a middle column's bytes would misalign every pre-drop
  row's positional decode. The tombstone keeps the slot; ~7 row-handling
  functions became `!dropped`-aware (project/order/column_index/apply_defaults/
  not_null/checks/unique_sets). **ADD COLUMN** appends physically and relies on
  `decode_row` filling missing trailing columns with the coerced DEFAULT/NULL
  (no heap rewrite). **Transactional DDL is request-level only**: `execute_sql`/
  `run_bound_plans` snapshot `control.catalog_root` and restore via
  `Engine::restore_catalog_root` on any statement failure (the catalog persists
  eagerly and is non-MVCC — the documented M1 limitation — so this manual
  restore is the rollback). **Crash-safe user-txn-scoped catalog redo/undo
  through `recovery.rs` is deliberately deferred** (Core-lane territory); the
  snapshot/restore mechanism is in place for whoever wires it. `DROP`/`TRUNCATE`
  orphan heap pages until Phase 1's FSM lands. `lib.rs` change was a minimal
  additive guard on `execute_sql` + one helper — no restructuring.
- **P2.d (SERIAL).** `ColumnConstraints.identity` + `TableDef.serial_next`
  (durable per-column counter in the catalog blob; crash-safe via the catalog's
  own WAL-logged persist). `exec_insert::fill_serials` allocates before DEFAULT
  fill. Explicit value honored as-is, counter *not* advanced past it (Postgres
  `SERIAL` semantics). Single-writer ⇒ no duplicates. Persist-per-allocation
  (batching is a future perf item).
- **P2.e (prepared statements + bind params — injection surface closed).**
  `Literal::Param(usize)` + `logical::bind_params` substitutes every `$n` with
  the caller's value **before** the plan reaches the executor, so a Param never
  reaches encoding/comparison/wire (defensive unreachable arms on the 3
  exhaustive `Literal` matches). `Engine::execute_sql_params` is the
  injection-safe entry point; `prepare()`/`execute_prepared()` (new `Prepared`
  type) give parse-once/execute-many; both go through a shared
  `run_bound_plans`. Server: `SqlRequest.params` + `json_to_literal` +
  `EngineHandle::execute_sql_params` (writer-thread command); `POST /sql`
  documents `params`. **All parameterized paths bind before RLS/execute**, so a
  value is always data.

### P3.a — durable paged WAL-logged B-Tree (Core lane, branch `durable-storage`, 2026-07-08)

First checkpoint of **Phase 3** (`docs/backlog/phase3_durable_storage.md`), the
Core-lane worktree that begins the durable-index / big-file work. Full entry +
benchmark in `PROGRESS.md`'s P3.a section. What a future reader most needs:

- **`DiskBTree` replaces the in-memory `BTreeIndex`.** Nodes are pages in the
  shared page store carrying the standard 28-byte page header (so the buffer
  pool's CRC + D5 machinery applies unchanged); the B+tree payload lives in the
  body. New `PAGE_TYPE_BTREE`. Leaf/internal/meta node kinds tagged in `body[0]`;
  keys are `OrderedValue` (compared in memory after decode, so the byte encoding
  need not be order-preserving). Leaves are right-linked (`next_leaf`) for range
  + duplicate-key walks.
- **WAL model: new redo-only `WAL_INDEX`** (full node-page image, `slot ==
  u16::MAX`, same shape as `WAL_FPI`/`WAL_VACUUM` full-image). Each
  `insert`/`remove` is **one mini-txn** bracketing every page it touches (leaf
  write, or split-chain + root-repoint + meta-page update). Recovery redoes all
  pages of a committed index mini-txn or none — atomic, idempotent, LSN-stamped;
  recovery uses `restore_page_image` (ensures the file is sized, no CRC/LSN gate
  — last-writer-in-LSN-order wins, and index pages never overlap heap pages).
- **No undo — proven safe, not swept under the rug.** An index entry is a *hint*
  re-validated against MVCC in `try_exec_select_btree`, so stale/extra entries
  are harmless. The only dangerous case (a committed, visible heap row lacking
  its index entry — a false negative) can't happen because the index mini-txn
  fsyncs during statement execution, *before* the user txn's `WAL_TXN_COMMIT`.
  `tests/btree_mvcc.rs` proves an aborted insert never surfaces via the index.
- **Stable meta page ⇒ O(1) open.** A per-index meta page (id stored once in
  `ColumnDef.index_root`, never changes) holds the current root; a root split
  repoints it in place — never a catalog rewrite. `Engine::open` reads catalog →
  meta → root; **no heap rescan.** Benchmark (`benches/durable_index.rs`) shows
  B-Tree reopen flat vs. a still-rebuilt HNSW rising with rows.
- **Moved off the async worker** (`index_worker.rs` — `IndexKind::BTree` now
  `unreachable!` there). Executor writes durable entries inline
  (`apply_durable_btree_writes`, called from `exec_insert`/`exec_update`); reads
  via `DiskBTree::search`. **Vacuum scrubs the durable tree directly**
  (`DiskBTree::remove`) — it reads each dead row's indexed value via the new
  `Heap::get_raw` **before** `mark_dead`, since the slot must still be LIVE to
  read the body, then removes before compaction promotes the slot to reusable
  (the same M10.c aliasing gate, extended to the on-disk index).
- **v1 limits (documented):** deletes don't merge/rebalance underfull nodes (an
  emptied leaf stays linked — never wrong, tree only grows); one fsync per key
  insert (indexed INSERT pays heap fsync + one index fsync; batched in the
  server's deferred-sync mode); `DROP INDEX` pages leak like `DROP TABLE` pages.
- **New crash point P13** (`tests/crash/main.rs`): build past several splits,
  wipe the entire data file, recover the whole tree from the WAL. Harness 14→15.
  **Test count: 316 → 324** default-feature unit tests (+ P13). Only-mechanical
  test updates elsewhere: `btree_mvcc`/`index_rebuild`/`lib.rs` dropped the now-
  obsolete `IndexStatus::Ready` polling for BTree (durable ⇒ always consistent,
  no async status), and `index_worker.rs`'s `remove_rows` test switched from the
  removed BTree variant to FullText.

### P3.b — durable full-text + edge index; CSR retired (Core lane, branch `durable-storage`, 2026-07-08)

Second Phase 3 checkpoint. Full entry + benchmark in `PROGRESS.md`'s P3.b
section. The load-bearing insight and what a future reader most needs:

- **A full-text index and an edge index are both "key → many RowIds" — exactly
  what `DiskBTree` already is.** So P3.b added *no* new structure: it reuses
  P3.a's `DiskBTree` + `WAL_INDEX` machinery verbatim (**no new record kind /
  page type / `FORMAT_VERSION` bump**).
- **Full-text durable.** `apply_durable_index_writes` (renamed from
  `apply_durable_btree_writes`, now handling BTree *and* FullText) tokenizes the
  text (`fulltext::tokenize`, made `pub(crate)`) and inserts one
  `(OrderedValue::Text(token), RowId)` entry per token. `CREATE INDEX ... USING
  FULLTEXT` builds/backfills the tree like BTree. New **`Engine::search_fulltext`**
  read path (tokenize query → intersect each token's `search_eq` posting list,
  AND-only → MVCC-resolve). The durable full-text index previously had *no* query
  surface at all — this is the first one.
- **Edge index durable.** `__edges__.from_id` is now a real durable `BTree`
  index. `ensure_edge_index` (replaces `rebuild_edge_index`) at open
  creates-or-loads it and returns the meta page, cached on the Engine as
  `edge_index_meta`. `create_edge`/`delete_edge` maintain it via `DiskBTree::
  insert`/`remove(OrderedValue::Int(from_id), rid)`; `edges_from` + the Cypher
  executor read via `search_eq` (`graph/executor::execute` now takes a
  `PageId`, not `&EdgeIndex` — a `Copy` value, so no borrow clash with
  `&mut ctx.pool`). The in-memory `EdgeIndex` struct is deleted;
  `graph/index.rs` keeps only `resolve_candidates_batched`. Vacuum scrubs the
  edge index through the generic durable-index path (from_id is now
  `IndexKind::BTree`, so the `remove_rowid`-by-physical-RowId special case is
  gone — vacuum re-derives from_id from the dead row via `get_raw`).
- **CSR retired (evidence-based, not a §3 reversal).** `csr_index.rs` was
  consulted by *no* read path after M7's traversal-uses-CSR wiring was reverted,
  and adjacency is now durable via the edge index — so `rebuild_csr_index` +
  the `IndexedColumn::Edge` warm-keeping sends were removed. The module + its
  `benches/graph.rs` measurement stay (still a valid CSR-vs-naive benchmark) but
  are unwired from the runtime.
- **The async index worker now serves only the vector (Hnsw) index.**
  `index_worker.rs` shed `FullText`/`Csr`/`Edge`/`Text`/`Ordered` and the CSR
  debounce machinery; `SecondaryIndex`/`IndexedColumn` are single-variant now
  (their `let ... else`/`match` sites simplified to irrefutable binds). P3.c will
  make vector durable too and retire the worker entirely.
- **Crash points P14 (full-text) + P15 (edge index)** at the Engine level:
  commit, "crash" (drop, no checkpoint), reopen, query works with no rebuild.
  Harness **15 → 17**. No `FORMAT_VERSION` change.

### P3.c — on-disk vector index spike + a DiskBTree duplicate-key bug fix (Core lane, branch `durable-storage`, 2026-07-08)

Third Phase 3 checkpoint, delivered as the **spike** the blueprint mandates
(research-grade; validate recall before committing). Full entry + numbers in
`PROGRESS.md`'s P3.c section and `docs/design/p3c_vector_spike.md`.

- **Chose on-disk IVF-Flat** (`src/disk_vector.rs`, `DiskIvfIndex`). The reuse
  insight (same as P3.b): an IVF cell posting list `cell_id → [RowId]` is
  *exactly* a `DiskBTree`, so the durable core is already built; the only new
  in-RAM state is the centroid table (bounded, not O(corpus)). Vectors stay in
  the heap (exact re-rank). DiskANN/Vamana parked as a higher-recall option
  behind the same interface. **Recall validated: recall@10 = 1.000 at nprobe=4**
  vs. brute-force ground truth (`benches/vector_recall.rs`), 4 KB RAM, 24 ms
  build — vs. the in-RAM HNSW's 30 s build for 1,200 vectors (the M2
  rebuild-per-upsert pathology, quantified). **Production wiring (CREATE INDEX →
  durable, NEAR reads it, centroid persistence, a new crash point P17) is the
  follow-up PR.**
- **The spike found + fixed a real `DiskBTree` bug that also affected P3.a/P3.b.**
  IVF recall capped at 0.912 even probing all cells → a duplicate-key run
  **straddling a leaf boundary** was under-returned: `search_eq` (and `remove`)
  could land mid-run via `find_leaf` and stop early. **Fix:** `find_leaf` now
  descends to the *leftmost* candidate leaf (routes left on `key <= separator`,
  since a separator is the first key of its right subtree) and `search_eq`/
  `remove` walk the leaf `next`-links until a key strictly greater than the
  target appears. Regression: `btree_index::
  heavily_duplicated_key_spanning_leaves_returns_all` (3,000 dups over ~7
  leaves). Real-world impact this closed: a full-text token in many docs, a
  graph hub with many edges, or a BTree value on many rows would have silently
  returned an incomplete set. (The insert path keeps `<` routing so new
  duplicates append after existing ones — only reads needed leftmost descent.)

### P3.c (production) — durable vector index live; async worker retired (Core lane, branch `p3c-vector-production`, 2026-07-09)

Promotes the spike's `DiskIvfIndex` into the live vector index — **closing Phase 3**.
Full entry in `PROGRESS.md`'s "P3.c (production)" section. What a future reader
most needs:

- **`DiskIvfIndex` is now a stateless handle over a stable meta page** (id in
  `ColumnDef.index_root`), *exactly* mirroring `DiskBTree`. The meta page (a
  distinct `IVF_META_MAGIC` body on a `PAGE_TYPE_BTREE` page) stores
  metric/dim/nlist/nprobe + the postings tree's meta page + the head of a
  **WAL-logged centroid page chain**. Every op reloads the bounded (`O(nlist·dim)`)
  centroid table from the buffer pool — so **centroids are crash-recovered, never
  recomputed**, and open is O(1). All pages use `WAL_INDEX` full-page images, so
  recovery is identical to `DiskBTree` — **no new record kind / page type /
  `FORMAT_VERSION` bump** (same reuse pattern as P3.b/P3.d).
- **`CREATE INDEX ... USING HNSW` (+ a new `USING IVF` parser alias) builds it.**
  `Hnsw` now *denotes* the durable IVF-Flat index (HNSW-the-graph retired); the
  catalog/SQL keyword is kept for compatibility. `exec_create_index` trains
  centroids from the committed rows (`ivf_params`: `nlist ≈ √rows` capped at 256,
  `nprobe` recall-favoring), persists, inserts each row. **Empty-table create →
  one origin cell (nlist=1)** = correct-but-flat brute force until re-created
  (documented; re-train-as-maintenance is a follow-up).
- **`NEAR` routes through it** (`exec_select_near`): `DiskIvfIndex::candidates`
  probes the nearest cells' posting lists → fetch rows from the heap → **exact
  re-rank** by the index metric → same MVCC/RLS/AND'd-predicate re-check
  (unchanged over-fetch-then-filter contract). `apply_durable_index_writes`
  maintains it on INSERT/UPDATE; vacuum's aliasing gate scrubs it via
  `DiskIvfIndex::remove`.
- **The async index worker is fully retired** — its last user was the in-RAM
  HNSW. `rebuild_secondary_indexes` deleted; `src/index_worker.rs` removed;
  `IndexHandle`/`IndexMsg`/`SecondaryIndex`/`IndexedColumn`/`build_indexed_columns`/
  `send_index_upserts` gone; `ExecCtx` lost its `index_worker` field; `Engine`
  lost its worker field + `Drop`. **`IndexStatus` moved to `catalog.rs`** and
  `Engine::index_status` now computes it from the catalog (a durable index is
  always `Ready`) — the REST `GET /indexes/:table/:column/status` route and DTOs
  are unchanged.
- **Recall parity proven** (`benches/vector_recall.rs`, extended with a
  20,000×64d sweep + a reopen-by-meta-page check): recall@10 = **1.000** matching
  the HNSW baseline's 1.000, bounded RAM (4 KB / 36 KB), and a fresh handle over
  the same meta page answers identically → no rebuild on open. Crash point **P17**
  (harness 18 → **19**): multi-cell index survives a crash, exact nearest + top-5
  recovered.
- **Gate met:** `Engine::open` is O(1) for **all** index types (B-Tree/full-text/
  edge as `DiskBTree`, vector as `DiskIvfIndex`) — zero rebuilding. The moat is
  durable.

### P3.d — large-object (big-file) storage (Core lane, branch `durable-storage`, 2026-07-08)

Fourth Phase 3 checkpoint. Full entry in `PROGRESS.md`'s P3.d section. Key points:

- **The design decision: large objects are ordinary heap rows, not a bespoke
  overflow format.** A blob is a sequence of ~7 KiB **chunk rows** in a `__lobs__`
  system heap table (`lob_id, chunk_no, data BYTEA`), indexed by a durable
  `DiskBTree` on `lob_id` (reuses P3.a). Because chunks are ordinary MVCC/WAL
  rows written under the caller's `xid`, the blob is **atomic with the
  transaction**, **crash-recovered**, and **vacuum-reclaimable** with *zero new
  storage format* — the same "new durable state is always ordinary heap rows"
  pattern M3/M4 used for `__edges__`/`__events__`.
- **Streaming (the "without OOM" gate):** `Engine::put_large_object(xid, impl
  Read)` inserts one chunk at a time pulled from the reader;
  `read_large_object(xid, lob_id, impl Write)` fetches one chunk row at a time
  into the sink. One ~7 KiB chunk resident at a time on both paths — a multi-GB
  value never loads whole. `lob_id` from a counter derived at open from
  `__lobs__`'s max (`derive_next_lob_id`, mirrors `next_event_seq`).
- **Files:** `src/large_object.rs` (`LobStore` + `ensure_lobs_table`), `lib.rs`
  (Engine API + open wiring + fields `lob_index_meta`/`next_lob_id`),
  `tests/large_object.rs`, crash point **P16** (harness 17→18). No
  `FORMAT_VERSION` bump; D4 tuple format unchanged.
- **Deferred (documented):** transparent BYTEA-toast of a large inline column
  value; streaming REST upload/download routes (server-side streaming through
  the single writer thread needs a chunked-command path — real design work, not
  buffering a whole blob in the writer).

### Phase 4 — query power (SQL lane, branch `query-power`, 2026-07-09)

The SQL lane's second phase: real SQL over a physical operator tree. Full entry
+ TPC-H-subset benchmark in `PROGRESS.md`'s Phase 4 entry. What a future reader
most needs:

- **The load-bearing design decision: additive routing.** `LogicalPlan::Select`
  is **unchanged** for the trivial single-table filter/project case (it still
  feeds the concurrent-read fast path `plan_is_concurrent_read` and every pre-P4
  test). Anything richer — a join, aggregate, GROUP BY, ORDER BY, DISTINCT,
  LIMIT, subquery, IN-list, or CTE — the parser routes into a new
  **`LogicalPlan::Query(QuerySpec)`** that a Phase-4 planner turns into a
  physical `PlanNode` tree the executor runs. This is why the merge stayed clean
  and the 258 pre-P4 tests never moved.
- **A separate expression type `QExpr`** (qualified columns, OR/NOT/IS NULL,
  aggregates, subqueries) lives beside the flat `Expr` rather than extending it,
  so Phase-4 work only ever adds arms to its own matches — the battle-tested
  single-table `Expr` (used by RLS, CHECK, DTOs) is untouched. RLS composes with
  joins by AND-ing each base relation's policy into the query's residual filter,
  qualified to that relation (`QuerySpec::apply_rls_from`) — the executor still
  never learns RLS exists.
- **New modules (all SQL-lane, additive):** `sql/query.rs` (QuerySpec/QExpr),
  `sql/plan.rs` (PlanNode tree + planner + QExpr eval, reusing
  `executor::compare`), `sql/join.rs` (hash join w/ Grace spill-to-disk,
  sort-merge, block nested-loop), `sql/query_exec.rs` (the driver: base scans +
  index-nested-loop probe the durable B-Tree; a Runner materializes CTEs once
  and executes correlated subqueries per outer row via literal substitution,
  caching uncorrelated ones), `sql/aggregate.rs` (hash aggregation, SQLite-
  compatible result typing), `sql/sort.rs` (in-memory + external merge sort),
  `sql/optimizer.rs` (ANALYZE-driven cost model + Selinger DP join order +
  index-vs-scan), `sql/statistics.rs`, `sql/explain.rs`.
- **P4.d statistics are durable and never recomputed on open.** `ANALYZE
  <table>` scans + computes `TableStats` (row count, per-column distinct/null/
  min/max/equi-depth-histogram) stored in a **`Catalog`-side map**, not on
  `TableDef` — a deliberate choice so adding stats touched only `catalog.rs`, no
  storage-core (`large_object.rs` is off-limits) or other-lane `TableDef`
  constructor. Persisted via the catalog's existing WAL-logged page write, with
  a **backward-compatible catalog blob** (`{tables, stats}`; old bare-map
  catalogs still load). The optimizer **engages only when every base relation is
  an ANALYZEd plain table and the join tree is inner/cross-only** — otherwise it
  falls back to the rule-based `plan_from` (which keeps P4.a's index-nested-loop
  join). So an un-ANALYZEd query behaves exactly as before P4.d.
- **Correctness is differential vs SQLite** (rusqlite `bundled`, a **dev-dep
  only** — the sync invariant `cargo tree -p unidb --no-default-features
  --edges normal` still has no tokio/reqwest/axum). `tests/{join,aggregate,
  subquery,optimizer}.rs` compare result multisets/ordered rows against SQLite
  on shared data; `tests/explain.rs` asserts the plan reflects the chosen
  operators (incl. the index-vs-scan crossover). Unit tests in `sql/optimizer.rs`
  assert IndexScan-vs-Scan selection directly.
- **No `FORMAT_VERSION` bump, no new crash point** — Phase 4 added no new
  storage mechanism (stats ride the existing catalog page; joins/aggregates are
  read-side). Crash harness stays **19**.
- **Known limits (documented, not silent):** no window functions / recursive
  CTEs / FULL OUTER + USING + NATURAL joins; ORDER BY resolves an output-column
  name or 1-based position (not arbitrary expressions) in v1; join keys are
  compared by exact encoding (declare matching key types for cross-type numeric
  joins); the optimizer emits hash joins for reordered joins (INLJ comes from
  the rule-based path); and **the catalog is still a single ~8 KiB page blob**,
  so a very wide ANALYZEd schema can overflow it — a multi-page catalog is
  tracked tech debt (histogram buckets were kept at 8 to reduce the pressure).

### Design note: xid reuse after checkpoint — a real M1-era bug, found and fixed during M5

Found by manually smoke-testing the new REST server (M5.b), not by any
automated test: `curl` through `/sql` to commit several transactions
(observed xids up to 15), `POST /checkpoint`, restart the server, and the
very first new transaction was issued `xid=1` — already used. Root cause:
`TransactionManager::recover_next_xid` (`txn.rs`) resumes the xid counter
by scanning the WAL for `WAL_TXN_BEGIN` records and taking `max + 1` — a
correct approach *only* if those records are still in the WAL.
`checkpoint::run` (`checkpoint.rs`) truncates every WAL record before the
checkpoint LSN, and in ordinary use that's *every* prior transaction's
begin record, since a checkpoint only ever runs after they've all
committed. So the very first `Engine::open` after any checkpoint had
nothing left to scan and silently defaulted to `1`.

**Why no existing test caught this:** `lib.rs::xid_counter_survives_reopen`
(M1.a) commits a transaction, calls `flush()`, then reopens — `flush()`
only flushes dirty pages, it never truncates the WAL, so the
`WAL_TXN_BEGIN` record was always still there for that test's `recover_
next_xid` call. No test in M1-M4 ever combined "commit, checkpoint,
reopen" — the crash-injection harness's own checkpoint tests (P2/P4)
check that *committed data* survives, not that *xid continuity* survives,
and M2-M4's own reopen tests all use `flush()` for the same "just persist
dirty pages" reason, never `checkpoint()`. M5's REST server was the first
code path in this project's history to actually call `checkpoint()`
against real traffic and then get reopened — an honest example of a gap
that a new *usage pattern* surfaces even when every individual piece
(`checkpoint`, `recover_next_xid`, WAL truncation) was independently
correct and independently tested.

**Fix:** persist `TransactionManager`'s current `next_xid` (new `pub fn
next_xid(&self) -> Xid` accessor) into the control file at every
checkpoint, captured *before* `wal.truncate_before` runs. Control file
grew from 36 to 44 bytes (`next_xid: u64` at `[32..40]`, crc moved to
`[40..44]`), `FORMAT_VERSION` bumped 2->3 — a D3/D9-locked-decision
change, confirmed with the user before implementing (they chose "fix now,
as its own commit" over "note it and keep going with M5"). `Engine::open`
now resumes at `max(WAL-scan result, control.next_xid)`: correct whether
or not a checkpoint ever ran, and correct even if a future scenario
somehow has the WAL know about a *higher* xid than the last checkpoint
recorded (e.g. transactions active on the WAL side after the last
checkpoint but not yet checkpointed themselves).

**Severity note, stated plainly for a future reader:** this was silent
data-corruption-class, not a panic or an error return — a reissued xid
could collide with or be misordered relative to a prior committed xid
still referenced by existing tuples' `xmin`/`xmax`, producing wrong query
results with no error anywhere. Fixed immediately given that severity,
not deferred as "M5 tech debt," even though it isn't part of M5's actual
feature scope.

### Design note: WAL-tailing is a dead end for the event queue — copy events into an ordinary table instead (M4.a)

The M4 plan's central finding, confirmed by reading source before
committing to a design, not assumed: a queue built by tailing the live WAL
directly cannot work. Two independent reasons. First, `checkpoint.rs::
run()` truncates the WAL unconditionally once dirty pages are flushed —
there is no registry of readers, no lag concept, nothing that would let a
slow consumer hold truncation back (which would be D5-adjacent bad news
anyway — WAL retention and page-flush timing are not supposed to depend on
external readers). Second, WAL records don't even carry a table
identifier (only `page_id`/`slot`), so a consumer reading raw WAL couldn't
tell which table's row it's looking at without also consulting the
catalog's page-list-to-table mapping at read time — fragile, and still
wouldn't solve the truncation problem.

The resolution: `sql::executor::send_event_capture` copies the row into an
ordinary, durable `__events__` heap table **at write time**, synchronously,
under the writing transaction's own xid — the same "just an ordinary
system table" trick M3's `__edges__` used, for the same reason (`TableDef`
has no "kind" field distinguishing system vs. user tables, so
`__events__`/`__consumers__` get full MVCC versioning, WAL durability, and
`SELECT * FROM __events__` queryability for free). Once this copy exists,
`checkpoint.rs` needs zero changes and D5 is untouched — WAL truncation is
*structurally* incapable of caring how far behind a consumer is, because
the event no longer lives only in the WAL. Consumer lag's only consequence
is `__events__` growing until `Engine::vacuum_events()` (M4.c) reclaims
what every registered consumer has acknowledged past.

### Design note: event capture must be inline, not a commit-time hook — and the risk that surfaces from that choice (M4.a)

A commit-time hook reading `TransactionManager`'s accumulated `undo_log`
was considered and rejected before implementation, not after finding a
bug. The trap: capturing events either before or after `TransactionManager
::commit()`'s own WAL fsync creates a window where the event and the
underlying data-commit could disagree about whether the transaction
actually committed — exactly the kind of subtle ordering bug this
project's WAL-before-page discipline (D5) exists to prevent elsewhere.
Capturing inline, under the same xid, as an ordinary `heap.insert` into
`__events__` sidesteps this entirely: confirmed against source (not
assumed) that `Heap::insert`/`update`/`delete` never call `record_undo`
themselves — every existing call site (`exec_insert`, `exec_update`,
`exec_delete`, `create_edge`, `delete_edge`, `ack_events`) does so
explicitly right after the `heap` call, and `TransactionManager::abort`
replays `UndoAction::Insert`/`XmaxStamp` purely by physical `(page_id,
slot)`, with zero knowledge of which table or purpose they belong to. So
`send_event_capture` needed nothing new in `txn.rs` at all — just the same
`heap.insert(...)?; ctx.txn_mgr.record_undo(...)?;` two-line shape every
other write path already uses. This is what makes the "zero new
abort-path code" claim in `PROGRESS.md` literally true, not aspirational.

**The risk this surfaces, worth stating plainly:** forgetting the event
row's `record_undo` call would be a *silent* correctness bug, not a
compile error — `mvcc::is_visible` doesn't distinguish "aborted but never
undone" from "committed" (per M1.a's own design note), so a missed call
would make an aborted transaction's event durably *visible* to every
future consumer, forever, with no test failure anywhere near the bug's
actual location. This is why the abort-visibility test
(`aborted_transaction_event_is_self_visible_then_invisible_to_fresh_txn`)
was written in M4.a itself, immediately alongside `send_event_capture`,
rather than deferred to M4.d's milestone-level MVCC test — the same
"catch it close to the code" discipline M2/M3's MVCC tests already
established, applied one checkpoint earlier than usual specifically
because of how easy this particular mistake would have been to miss.

### Design note: `next_event_seq` lives on `ExecCtx` as a field, not threaded through as an extra argument (M4.a) — a deliberate deviation from the approved plan

The approved M4 plan explicitly favored mirroring M3.c's `edge_index`
precedent: pass `next_event_seq` as an extra function argument, keep
`ExecCtx` "pure storage/txn infra." Implementation found this doesn't fit
the actual call graph and deviated, for a concrete reason: `edge_index`
only ever needed to reach *one* top-level entry point
(`graph_executor::execute`, called directly from `Engine::execute_cypher`)
— an extra argument there is a one-line, one-call-site change. Event
capture, by contrast, must reach the *deeply nested private* `exec_insert`
/`exec_update`/`exec_delete` functions, which are only reachable through
`sql::executor::execute(plan, ctx)`. Threading an extra argument through
would mean changing `execute()`'s own signature and therefore every one of
its call sites (`Engine::execute_sql`, `Engine::execute_cypher`, and the
test `Harness::exec_as`) — strictly more invasive than the plan's stated
goal of minimizing touch points. `ExecCtx` already has exactly this shape
of exception: `index_worker: Option<&IndexHandle>` exists on `ExecCtx`
for the identical reason (`send_index_upserts` is called from the same
nested private functions). Adding `next_event_seq: &'a mut u64` alongside
it follows the *existing* precedent on this exact struct rather than
inventing a new, harder-to-thread mechanism to preserve a purity goal the
struct had already given up on for the same underlying reason. Recorded
here as a real-time design correction against a written, approved plan —
not a silent divergence.

### Design note: per-edge locking needed zero new code (M3.b)

The M3 plan flagged a real risk to check before assuming it: does graph
edges' shared use of `Heap`/`LockManager` alongside ordinary tables need a
new `RecordKind` variant (e.g. `GraphEdge`) so edge locks can't collide
with row locks in unrelated tables? Verified false. `RecordId::row(page_id,
slot)` (`lockmgr.rs`) packs `(page_id << 16) | slot` into a `u64` lock key.
`PageId` is allocated once, globally, from a single shared `BufferPool`
(`pool.alloc_page()` — see `bufferpool.rs`), **not per-table** — every
table in the database, including `__edges__`, draws its pages from the
same counter. So two rows can only ever produce the same lock key if
they're the literal same physical tuple version; there is no cross-table
collision possible even in principle, and adding a `GraphEdge` `RecordKind`
variant would have been solving a problem that doesn't exist.

`Heap::update`/`delete` already call `LockManager::try_acquire_write`
before any mutation, unconditionally, regardless of which table's `Heap`
handle is calling — since `create_edge`/`delete_edge` (M3.a) reconstruct an
ordinary `Heap::from_pages` against `__edges__`'s catalog page list and
call the same `heap.insert`/`delete` every SQL statement uses, they
automatically inherit the exact same conflict detection, first-committer-
wins semantics, and lock release-on-commit/abort behavior M1.b already
built and tested for ordinary rows. `tests/graph_locking.rs` proves this
end-to-end rather than just asserting it from code inspection: concurrent
edge deletes conflict immediately (D12, no blocking), an edge lock and an
unrelated table's row lock never collide, and locks release correctly on
both commit and abort.

One test-writing gotcha worth recording: `heap.rs::delete`'s two distinct
conflict checks (an active lock from another current transaction, vs. an
already-dead row whose deleting transaction has since committed and
released its lock) **both** return the same `DbError::WriteConflict`
variant — there is no way to distinguish "blocked by a live lock" from
"row already gone" from the error shape alone (by design, per `heap.rs`'s
own doc comment; see M1.b's design note above for why a separate
commit-time recheck was found to be unnecessary in the first place). A
test asserting "must not be a lock conflict" after the original lock
holder already committed is wrong — it's still correctly a
`WriteConflict`, just for the other reason. Fixed by asserting
`holder_xid` matches the *expected* xid instead of trying to distinguish
error variants that were never meant to be distinguishable.

### Design note: the batch-latch adjacency scan is a real, large win (M3.b)

Measured, not assumed, per CLAUDE.md §6: `benches/graph.rs`'s
`adjacency_scan` group compares one-`fetch_page`-per-candidate resolution
(`resolve_naive`, kept only in the benchmark for comparison) against the
shipped `resolve_candidates_batched` (M3.a) on a synthetic hot hub. Edge
rows are small enough that ~128 fit per 8 KiB page (1,000 edges → 8 distinct
pages; 10,000 edges → 78), so grouping candidates by `page_id` collapses
roughly 128 redundant `fetch_page` calls into one:

| Hot hub size | naive | batched | speedup |
|---|---|---|---|
| 1,000 edges (8 pages) | 879 µs | 94.3 µs | ~9.3x |
| 10,000 edges (78 pages) | 9.06 ms | 930 µs | ~9.7x |

This confirms `BufferPool::fetch_page`'s per-call page copy (see M3's plan
research) is a real, non-negligible cost at hot-hub scale, and that
grouping by page — not some more elaborate scheme — already captures
nearly all of the available win, since the speedup closely tracks the
edges-per-page ratio.

### Design note: the Cypher subset reuses ExecCtx via an extra argument, not a new field (M3.c)

The plan's original sketch had `graph::executor::execute` take just
`(query, ctx: &mut ExecCtx)`, matching `sql::executor::execute`'s shape
exactly. In practice, the index fast path needs read access to `EdgeIndex`,
which `ExecCtx` (defined in `sql/executor.rs`) has no field for. Two
options were considered: (a) add an `edge_index: Option<&EdgeIndex>` field
to `ExecCtx` itself, mirroring how `index_worker: Option<&IndexHandle>` was
added there in M2; or (b) pass `edge_index` as a separate explicit
parameter to `graph::executor::execute` alongside `ctx`. Went with (b):
it keeps `sql::executor::ExecCtx`'s definition exactly what M1/M2 already
built (pure storage/transaction infra, no graph-specific field), and the
borrow checker is fine with it — `ExecCtx`'s fields are constructed as
individual `&mut self.foo` borrows in `Engine::execute_cypher`, none of
which touch `self.edge_index`, so a separate `&self.edge_index` borrow
coexists with the `&mut ExecCtx` cleanly (Rust's field-level disjoint
borrows, not `&mut self` as a whole).

One real, planned-for cross-module touch was needed to make the reuse
work: `predicate_matches`/`eval_expr` were private `fn`s in
`sql/executor.rs` (confirmed during planning, not assumed) and were
promoted to `pub(crate) fn` — the only change made to the SQL module for
all of M3.c. Everything else (`ExecCtx`, `ExecResult`, `decode_row`,
`Expr`/`CmpOp`/`Literal`) was already `pub`.

The `:TYPE` filter from the `MATCH` pattern and the parsed `WHERE`
predicate are AND'd together into one `full_predicate` before either
execution path runs — so both the index fast path and the full-scan
fallback apply type filtering and `WHERE` filtering through the exact same
`predicate_matches` call on every candidate, with no special-casing for
which source (index vs. scan) a row came from.

### Design note: M3's benchmark comparison — batch-latch closes almost the whole read-side gap with Postgres (M3.d)

Measured, not assumed, per CLAUDE.md §6: `benches/graph.rs`'s adjacency
scan against a real, isolated Postgres benchmark database (indexed
adjacency-list table, dropped after recording numbers — same discipline as
M2.d's pgvector run). The headline result: unidb's *batched* adjacency
scan (M3.b) is within ~1.6x of Postgres at 10,000 edges (930µs vs 568µs)
and effectively tied at 1,000 edges (94.3µs vs 98µs) — while the *naive*
pre-optimization scan would have lost by 9–16x. This is the clearest
evidence in the project so far that a targeted, measured optimization (not
a rewrite) can make the engine genuinely competitive on the workload it's
built for, not just "less bad than before." INSERT throughput still lags
Postgres by ~35x, but that gap is the same pre-existing per-statement
fsync cost M1/M2 already found — not something M3 introduced, and not
fixed here since it's out of this milestone's scope (see Open questions).

### Design note: EdgeIndex has no abort-time cleanup — proven safe, not swept under the rug (M3.d)

`tests/graph_mvcc.rs` (this milestone's single most important test, per
the plan) confirms a real, load-bearing property: `Engine::abort` undoes
the heap-level effects of a transaction (self-stamping xmax, per M1's
mechanism) but has no hook into `EdgeIndex` at all — `create_edge`/
`delete_edge` are the only two places that ever touch it, and neither is
wired into the generic commit/abort path. So an aborted `create_edge`
leaves a permanently stale entry in the index, forever, pointing at a
`RowId` whose tuple is now permanently dead. The test proves this is safe
(not just assumed safe): it confirms the inserting transaction sees its
own uncommitted edge via `edges_from` (proving the stale entry really
exists), aborts, then proves a fresh transaction's `edges_from` *and* an
equivalent Cypher query both correctly exclude it — because every
candidate is re-checked against the caller's MVCC snapshot before ever
becoming a result, regardless of what the index says. Notably simpler to
test than M2's equivalent (`vector_mvcc.rs`): `EdgeIndex` is synchronous,
so there's no "did the background worker catch up yet" race to poll for
before aborting — the index is guaranteed current the instant
`create_edge` returns.

### Design note: read-only transactions pay an unnecessary commit fsync (found in M1.d)

Running M1's benchmarks (`benches/load.rs`) turned up a real, previously
unnoticed inefficiency: point `SELECT` (a pure read, no writes at all) went
from 855ns in M0 to 3.05ms in M1 — a ~3,570x regression, far more than the
~2x expected from adding a transaction wrapper. Root cause:
`TransactionManager::commit()` unconditionally calls `wal.commit_user_txn()`,
which fsyncs, regardless of whether the transaction ever wrote anything. A
read-only transaction has nothing that needs to become durable, so this
fsync is pure waste — real databases (Postgres, SQLite) specifically avoid
writing WAL records for read-only transaction commits for exactly this
reason. **Not fixed in M1** (wasn't in the agreed scope, and fixing it
properly means checking `Transaction.undo_log.is_empty()` at commit time
and skipping `wal.commit_user_txn()`'s fsync — or the call entirely — when
true, which touches `txn.rs`'s commit path CLAUDE.md would want reviewed
rather than slipped in as a drive-by). Recorded in `PROGRESS.md`'s M1 entry
and flagged in Open questions below so it doesn't get lost before M2.

### Design note: no separate "commit-time recheck" needed for SI conflict detection (M1.b)

The plan called for two distinct conflict checks: an immediate lock-acquire-time
check, and a "commit-time first-committer-wins recheck" guarding the case where
the previous lock holder released via abort and something else slipped in
before this transaction's commit. Implemented `LockManager` (`lockmgr.rs`,
`RecordKind`/`RecordId` generic over future M2+ kinds, write-write only — no
read locks under MVCC) and wired `try_acquire_write` into `Heap::update`/
`delete` before the mini-txn begins. But because a lock is held for the
*entire* transaction lifetime (released only in `TransactionManager::commit`/
`abort`, never in between), no other transaction can successfully write to a
row this transaction touched until this one finishes — there is no race
window between "write" and "commit" for a separate recheck to catch in this
single-threaded engine. `Heap::update`/`delete` already run two checks that
together *are* the complete conflict detection: (1) `try_acquire_write`
catches another *currently active* xid (immediate abort, no waiting, D12);
(2) the existing `xmax != 0` check catches a row already superseded by a
transaction that has *since committed and released its lock* — a distinct
failure mode the lock table alone can't see once the holder is gone. Verified
by `lib.rs`'s `concurrent_update_aborts_second_writer_immediately`,
`commit_releases_lock_for_next_writer`, `abort_releases_lock_for_next_writer`.

### Design note: catalog is not MVCC-versioned; page-list tech debt fixed (M1.c)

Two deliberate scope calls made while building `catalog.rs`/the executor:

1. **Catalog rows are not MVCC-versioned.** DDL takes effect immediately and
   globally the moment `CREATE TABLE` returns — no snapshot isolation for
   schema, no rollback of a `CREATE TABLE` if the surrounding transaction
   later aborts. Building real snapshot-isolated DDL would require every SQL
   statement's catalog lookup to carry a snapshot and walk visibility,
   disproportionate to M1.c's actual goal (prove SQL works end-to-end). The
   catalog is persisted as a single `serde_json`-encoded blob rewritten to a
   fresh page on every change (`control.catalog_root` points at the latest
   one) — using `serde` here, unlike the rest of the on-disk format, is
   deliberate: schema metadata is infrequent control-plane data, not what
   D9's "no serde on the hot path" rule is protecting.
2. **Fixed a real latent bug while building table storage**: `Heap`'s page
   list was in-memory only (flagged as tech debt since M0/M1.a), meaning
   `scan()` would have silently returned nothing for a table's existing rows
   after every engine reopen. `TableDef.pages: Vec<PageId>` now persists
   each table's page list in the catalog, and `Heap::from_pages`/`page_ids()`
   let the executor reconstruct a working `Heap` handle each statement and
   detect growth to persist back. Verified by
   `executor::tests::table_survives_reopen_via_catalog_pages` and
   `tests::sql_survives_reopen`.

Also: there is no separate "physical plan" IR (`sql/physical.rs` from the
original plan was folded into `executor.rs`) — M1's grammar subset maps 1:1
from logical plan to execution step (single table, no joins), so a distinct
physical layer bound to schema would have been a premature abstraction for
this milestone; column-name resolution against `TableDef` happens directly
inside the executor instead.

RC's EvalPlanQual-style re-evaluation path (D12, sequenced after SI) is
**not implemented** — UPDATE/DELETE conflicts propagate as `WriteConflict`
regardless of isolation level. This is a tracked, documented gap (see
`sql/executor.rs`'s module doc), not a blocker for M1.c's "prove SQL works"
bar; it needs the executor's predicate evaluation to exist first, which it
now does, so it's ready to build whenever it becomes a real gap in practice.

### Design note: abort requires physical undo even in M1.a (not deferred to M1.b)

While implementing `txn.rs`, found that `mvcc::is_visible`'s snapshot check
(`is_committed_at_snapshot`: not-in-active-set-and-in-range ⇒ committed) has
no separate "aborted" concept — so a naive `TransactionManager::abort()` that
just flips txn state without reversing the tuple bytes would make an aborted
insert look committed to any snapshot taken after the abort. Fix: abort must
physically neutralize its own writes immediately, by self-stamping xmax on
any tuple it inserted (`xmax = its own xmin`, making it permanently
invisible — same code path as a normal delete-then-committed row) and
reverting any xmax stamp it applied back to 0. This reuses `is_visible`'s
existing committed/active distinction instead of adding a third state.
Implemented via `Heap::undo_insert`/`undo_xmax_stamp`, driven by an in-memory
`Vec<UndoAction>` on each `Transaction` (built up as `Heap` calls happen —
cheap, no WAL-decoding needed at runtime since the process is still alive).
Recovery's crash-time undo of an *incomplete* user transaction (no in-memory
state survives a crash) instead reconstructs ownership by decoding
`xmin`/xmax straight out of the WAL's redo bytes — see `recovery.rs`'s
two-phase pass (revert xmax-stamps first, then force-self-stamp inserts last,
so a row both inserted and re-superseded by the same aborted transaction
correctly ends up permanently dead rather than accidentally revived). This
same idempotent recovery pass is what makes crash-mid-abort safe too (P9,
`tests/crash/main.rs`): whether runtime abort never started, or crashed
partway through its own undo_log, recovery re-derives the same "incomplete
user txn" verdict from the WAL and re-applies the same idempotent undo.

### Design note: VECTOR(n) row encoding and parser plumbing (M2.a)

`ColumnType::Vector(u32)` carries a fixed dimension `n`, validated `> 0` at
both `CREATE TABLE` time (parser) and every INSERT/UPDATE (executor's
`coerce_value`/`decode_row`). Row encoding uses a new tag byte `5`:
`[dim:4 LE][f32 * dim, 4 bytes LE each]` — dimension-prefixed (not just
relying on the column's declared `n`) so `decode_row` can cross-check the
stored dimension against the schema and return a `DbError::SqlPlan` on
mismatch rather than silently misreading bytes or panicking. `f32`, not
`f64`: matches real embedding models' native precision and halves row size,
and matches `pgvector`/FAISS convention for the later Postgres+pgvector
benchmark comparison.

Parser plumbing required two `sqlparser` 0.62.0 specifics, both confirmed
against the vendored source before use (see plan file): `VECTOR(n)` has no
built-in AST type, so it arrives as `DataType::Custom(ObjectName,
Vec<String>)` — matched case-insensitively on the name, first modifier
parsed as `u32`. Bare `[0.1, 0.2, ...]` array literals parse unconditionally
under `GenericDialect` as `SqlExpr::Array`, unrelated to `VECTOR` — handled
by a new `convert_array_literal` that parses each element as `f32` (a
narrow fallback scoped to array-literal elements only; `convert_value`'s
general numeric path stays `i64`-only, unchanged).

Dimension validation is deliberately enforced in three independent places
(parser rejects `VECTOR(0)`; executor's `coerce_value` checks the literal's
length against the column at plan-execution time; `decode_row` re-checks on
every read) rather than trusting any single point — cheap, and each guards
a different failure mode (bad DDL, bad INSERT, corrupted/mismatched stored
bytes).

### Design note: instant-distance has no incremental insert — plan corrected (M2.b)

The approved M2 plan chose `instant-distance` partly on the assumption of
"native incremental insertion." That turned out to be wrong: checked against
the vendored 0.6.1 source, `Builder::build`/`Hnsw::new` only construct an
`HnswMap`/`Hnsw` from a full `Vec<P>`/`Vec<V>` at once — there is no public
method to add a single point to an already-built graph. Corrected design
(`src/vector.rs`): `VectorIndex` buffers every live point in a
`HashMap<RowId, Vec<f32>>` and rebuilds the whole HNSW graph from scratch on
every `upsert`/`remove`. This still satisfies CLAUDE.md's M2 goal ("row
write is the only synchronous cost") because the rebuild happens entirely on
the background worker thread — the foreground write path only ever sends a
channel message, same as the original plan intended. The tradeoff is
real, though: rebuild-per-upsert is O(n log n) per insert at the index
structure level, not O(log n) amortized the way true incremental HNSW
insertion would be. Not a correctness issue, and M2.d's benchmark table
(§6, "report honestly if it doesn't show negligible overhead") is exactly
where this gets evidence-based scrutiny rather than being assumed fine.
Distance metric: squared-root Euclidean (`pgvector`'s `<->` default), chosen
for the later benchmark comparison to be apples-to-apples.

### Design note: background worker never touches storage-layer types (M2.b)

`index_worker.rs`'s worker thread owns exactly one thing:
`Arc<RwLock<HashMap<(table, column), IndexEntry>>>`, built purely from
`IndexMsg` channel messages. It never receives a `BufferPool`, `Wal`, or
`Heap` handle — confirming the plan's core risk-mitigation choice held up
in practice. Two flows funnel through the *same* channel:
- **Rebuild-on-open**: `Engine::open` runs an ordinary begin/scan/commit
  read-only transaction (identical MVCC machinery to a `SELECT`) on the
  foreground thread, decodes each row via the existing `executor::decode_row`,
  and sends one `IndexMsg::Upsert` per row with a non-empty vector column,
  followed by one `IndexMsg::MarkReady` per indexed column once the scan
  finishes. This is what lets `IndexStatus` distinguish `Building` (worker
  still draining a backlog) from `Ready` (drained) — `MarkReady` is
  processed strictly after every `Upsert` sent before it, since it's the
  same FIFO channel.
- **Live upserts**: `sql/executor.rs`'s new `send_vector_upserts` runs once
  per inserted/updated row (not once globally), checking `ColumnDef.index`
  directly — zero cost for tables with no indexed column, satisfying "row
  write is the only synchronous cost."

**A new general catalog primitive was added ahead of its originally-planned
checkpoint**: `Catalog::set_column_index`/`Engine::set_column_index` (M2.b),
even though the plan placed "persist `ColumnDef.index`" under M2.c's
`CREATE INDEX` task. Justified narrowly: M2.b's own tests needed *some* way
to mark a column indexed to prove the worker pipeline end-to-end, and this
is exactly the catalog-persistence primitive M2.c's `CREATE INDEX` executor
code was always going to call internally (mirrors `set_rls_policy`'s
existing pattern) — M2.c only adds the SQL parsing, `LogicalPlan::CreateIndex`,
and immediate backfill-on-existing-rows on top of this, not a competing
mechanism. What M2.b's `set_column_index` deliberately does *not* do:
backfill already-committed rows immediately — an already-populated table
only gets indexed on the next `Engine::open`'s rebuild-on-open rescan.
`CREATE INDEX` (M2.c) will call `set_column_index` and *then* run its own
backfill scan, using the exact same rebuild logic factored out for reuse
(`send_vector_upserts_for_rebuild` in `lib.rs`).

**Known, accepted tech debt from this checkpoint** (parallel to M1's
"no vacuum" gap): `VectorIndex` has no removal-on-obsolescence path for
UPDATE. Since M1 UPDATE always creates a new `RowId` (never in-place), a
row's old vector value stays in the index forever, keyed by a `RowId` whose
tuple is now permanently dead. This is a correctness non-issue — a stale
candidate resolves to `NoVisibleVersion` at read time and gets filtered out,
exactly like MVCC's existing "no vacuum" story for the heap itself — but it
is an unbounded space leak under update-heavy workloads on indexed columns.
Tracked below, not silently dropped.

### Design note: CREATE INDEX's USING clause must precede the column list (M2.c)

`sqlparser` 0.62.0's `parse_create_index` only looks for an optional
`USING <type>` clause immediately after the table name — *before* the
`(column)` list, not after (confirmed by reading `parse_create_index`
directly, not guessed). So the SQL surface is
`CREATE INDEX idx ON t USING HNSW (embedding)`, not
`CREATE INDEX idx ON t (embedding) USING HNSW` (the latter is a
different, MySQL-specific trailing-options grammar path this project
doesn't hook into). `HNSW`/`FULLTEXT` arrive as `IndexType::Custom(Ident)`
since neither is a real SQL index type — matched case-insensitively, same
pattern as `VECTOR(n)`'s `DataType::Custom` fallback from M2.a.

### Design note: CREATE INDEX generalizes M2.b's rebuild/upsert plumbing, doesn't duplicate it (M2.c)

`exec_create_index` (`sql/executor.rs`) and `lib.rs`'s rebuild-on-open both
need the same "decode a row, pick out its indexed columns, build the right
`IndexedColumn` variant per column type" logic. Factored into one shared
function, `executor::build_indexed_columns`, so the
`ColumnType`/`IndexKind` → `IndexedColumn::{Vector,Text}` mapping exists in
exactly one place. `lib.rs`'s `rebuild_vector_indexes` was renamed
`rebuild_secondary_indexes` and generalized from "only scan `Hnsw` columns"
to "scan any indexed column" — necessary because a table with only a
`FullText` index would otherwise have silently lost its index on every
reopen (M2.b's version only ever looked for `Hnsw`). Caught and fixed in
the same pass as building `CREATE INDEX`, not left as a latent gap.

The one behavioral difference between the two entry points, by design:
`CREATE INDEX` (M2.c) backfills *immediately* (scans currently-committed
rows synchronously-enqueued, right there in the executor), while
`Engine::set_column_index` (M2.b's Rust-only API, kept for programmatic use)
still defers population to the next `Engine::open`'s rebuild. `CREATE
INDEX`'s validation (`IndexKind::Hnsw` only on `ColumnType::Vector`,
`IndexKind::FullText` only on `ColumnType::Text`) reuses the exact
`DbError::SqlPlan` error shape already established for vector-dimension
mismatches in M2.a — one consistent "bad plan for this schema" error
family, not a new one per feature.

### Design note: NEAR's over-fetch-then-filter execution and the MVCC re-check (M2.d)

`Expr::Near { column, query, k }` lives inside `Select.predicate: Option<Expr>`
— a predicate-shaped construct, not a new `LogicalPlan` variant — so
`apply_rls`'s existing AND-rewrite needed zero changes: `WHERE NEAR(...) AND
<rls policy>` composes for free, and `NEAR(...) OR x` is already rejected by
the existing AND-only `WHERE` grammar with no special case needed.

`exec_select` detects a top-level (or top-level-AND'd) `Near` via a small
`find_near` walk and dispatches to `exec_select_near`, which: (1) validates
the column actually has `IndexKind::Hnsw` on a `Vector` column — a clear
`DbError::SqlPlan`, not a silent full-scan fallback, for both "no index"
and "wrong index kind" cases; (2) takes a read lock on the worker's shared
`indexes` map and asks `VectorIndex::search` for `4x k` (or `k+20`,
whichever larger) candidates; (3) resolves each candidate `RowId` back to a
row via the *same* `Heap::get` + MVCC snapshot every other read path uses,
silently dropping any `NoVisibleVersion` result (superseded row, or a row
whose insert never committed); (4) runs the row through the *same*
`predicate_matches` a full scan uses, so any AND'd RLS/WHERE terms apply
identically. `eval_expr`'s `Expr::Near` arm always returns `true` when
re-evaluating a candidate that already came from the index — it does not
recompute distance — since proximity was already established by step 2;
that arm is *only* ever reached from this recheck path, never from a full
scan (which never dispatches into `exec_select_near` in the first place).

An index entry absent from the worker's map (e.g. `CREATE INDEX` just
enqueued its backfill and the worker hasn't processed the first message
yet) yields zero candidates, not an error — this is what
`IndexStatus::Building` is for. A genuinely bad `MarkReady` bug was found
and fixed in this pass: sending `MarkReady` for a column that had never
received a single `Upsert` (e.g. `CREATE INDEX` on an empty table) used to
silently no-op, since the handler only updated an *existing* map entry.
That left the column's status permanently stuck in `Building` once the
first live row finally arrived (its `Upsert` would create a fresh
`Building` entry that no later message ever flipped to `Ready`). Fixed by
having `MarkReady` carry the `IndexKind` and create an empty, already-`Ready`
entry if none exists — see `index_worker.rs`'s
`mark_ready_on_never_upserted_column_creates_ready_entry` regression test.

### Design note: no cross-statement RowId stability

Initially built `Heap::get` to walk the `prev_page`/`prev_slot` chain
backward looking for a visible version when the given `RowId` itself wasn't
visible. This doesn't work: the chain only points to *older* versions, so it
can never find a *newer* one, and two unit tests written against that
assumption failed for good reason. Removed the walk — `get` now does a
single direct visibility check against the exact given `RowId` and returns
`NoVisibleVersion` otherwise. This matches the M1 plan's explicit
simplification: **no stable row handles across statements**, even within the
same transaction that just updated the row. Callers (including the
transaction that just called `update`) must use the returned `RowId` or
re-scan, never reuse a pre-update one. `prev_page`/`prev_slot` still exists
and is populated — its purpose is version-history bookkeeping (recovery's
undo-ownership decoding, future vacuum), not reader traversal.

---

## What exists now

M0 modules, unchanged in location but several rewritten for MVCC in M1;
M1.c adds a whole new `catalog`/`sql` subsystem:

```
src/
  format.rs           — constants, endian helpers, WAL_TXN_* tags, Xid type (M1)
  error.rs            — DbError + Result type (thiserror); +12 M1 variants
  control.rs          — control file, with catalog_root field (M1, in active use since M1.c)
  mmap.rs             — ONLY unsafe module: PageFileMmap wrapper around memmap2
  page.rs             — slotted-page body; tuple header now 24 bytes (xmin/xmax/prev_page/prev_slot, M1)
  bufferpool.rs        — frames, pin/unpin, clock eviction, D5 enforced at flush/evict
  wal.rs              — mini-txn WAL (D2, unchanged) + user-txn WAL_TXN_BEGIN/COMMIT/ABORT (M1)
  mvcc.rs             — (new, M1.a) Snapshot + is_visible: pure MVCC visibility logic
  txn.rs              — (new, M1.a; extended M1.b) TransactionManager: begin/commit/abort
                         (now also releases locks), RC vs RR snapshot lifetime
  lockmgr.rs          — (new, M1.b) RecordKind/RecordId/LockManager: write-write conflict
                         tracking, no wait queue (D12 — SI aborts immediately, doesn't block)
  concurrency_hooks.rs — (new, M1.a) on_read/on_write no-op seam (D11)
  heap.rs             — (rewritten M1.a; extended M1.b, M1.c) MVCC-versioned insert/update/
                         delete/get/scan/from_pages/page_ids; update/delete call
                         LockManager::try_acquire_write first
  catalog.rs          — (new, M1.c) TableDef/ColumnDef/ColumnType/Catalog: table name -> schema
                         + page list, persisted as a serde_json blob, not MVCC-versioned
  sql/
    mod.rs            — (new, M1.c) module registration
    logical.rs        — (new, M1.c; extended M2.a, M2.c, M2.d) LogicalPlan/Expr/Literal/
                         CmpOp + apply_rls (the entire RLS mechanism is this one AND-rewrite
                         function); LogicalPlan::CreateIndex{table,column,kind} (M2.c);
                         Expr::Near{column,query,k} (M2.d, lives inside Select.predicate,
                         not a new LogicalPlan variant)
    parser.rs         — (new, M1.c; extended M2.a, M2.c, M2.d) wraps `sqlparser`'s
                         GenericDialect AST -> LogicalPlan; CREATE INDEX ... USING
                         HNSW|FULLTEXT (M2.c, note USING precedes the column list — see
                         design note above); NEAR(column,[...],k) parses unmodified as an
                         ordinary SqlExpr::Function (M2.d, zero grammar changes needed)
    executor.rs        — (new, M1.c; extended M2.a, M2.b, M2.c, M2.d) row-at-a-time
                         executor; hand-rolled row encoding (tag+value per column, tag 5 =
                         Vector, M2.a); no separate physical-plan IR (folded in);
                         exec_insert/exec_update send IndexMsg::Upsert for any indexed
                         column (M2.b); exec_create_index validates + persists +
                         immediately backfills (M2.c); build_indexed_columns is the one
                         shared column-type-to-IndexedColumn mapping used by both live
                         upserts and every backfill; exec_select_near (M2.d) over-fetch-
                         then-filter execution, reusing predicate_matches so MVCC/RLS/WHERE
                         all apply to NEAR results for free
  index_worker.rs     — (new, M2.b; extended M2.c) the engine's first background thread:
                         IndexMsg/IndexHandle/IndexStatus/SecondaryIndex{Vector,FullText},
                         owns Arc<RwLock<HashMap<(table,column), IndexEntry>>>, never
                         touches BufferPool/Wal/Heap
  vector.rs           — (new, M2.b) VectorIndex wrapper around `instant-distance`;
                         buffers points, rebuilds the HNSW graph on every upsert/remove
                         (no incremental insert in instant-distance's public API — see
                         design note above)
  fulltext.rs         — (new, M2.c) InvertedIndex: whitespace+lowercase tokenization,
                         AND-only multi-term intersection search, HashMap<String,Vec<RowId>>
                         postings
  checkpoint.rs       — flush dirty → checkpoint WAL record → update control → truncate WAL
  recovery.rs         — (extended, M1.a) mini-txn redo/undo (unchanged) +
                         incomplete-user-txn undo pass (decodes ownership from WAL redo bytes)
  lib.rs              — Engine API: begin/commit/abort + insert/get/update/delete take an xid;
                         + execute_sql/set_rls_policy (M1.c); owns LockManager + Catalog;
                         + index_worker: IndexHandle field, Drop impl shuts it down, spawned
                         and rebuilt-from-committed-rows in open() (M2.b)
tests/
  crash/main.rs       — 9 crash-injection tests: P1–P5 (M0) + P6/P7 (M1.a) + P9 (M1.b)
benches/
  load.rs             — INSERT / point-SELECT / UPDATE criterion benchmarks; M0 numbers recorded,
                        not yet re-run against M1's transactional API
```

Key design decisions confirmed in implementation (M0 + M1.a + M1.b + M1.c):
- D5 enforced: checked at `flush_page()` and in `find_victim()` eviction path only
- WAL uses length-prefix framing (u32 LE) + per-record CRC32; scan stops at corruption
- `mmap.rs` is the sole `#![allow(unsafe_code)]` module; rest of crate uses `#![deny]`
- All page/WAL integers are little-endian (D9); `FORMAT_VERSION` bumped 1→2 for the
  tuple header change (no migration path — M0 never shipped externally)
- Mini-txns (D2, per-statement) and user-txns (M1, multi-statement) are two
  independent ID spaces sharing one WAL wire format — `mini_txn_id`'s u64 slot
  doubles as the xid for `WAL_TXN_*` records
- `Heap::get`/`scan` do a single direct visibility check, no version-chain
  walk (see design note above — the chain only points backward, useless for
  finding a newer version; no cross-statement RowId stability by design)
- Abort/rollback works by physically self-stamping/reverting xmax, not by a
  separate "aborted" transaction-status check in the visibility path (see
  design note above)
- Locks are in-memory only, held for a transaction's full lifetime, released
  only at commit/abort — this is what makes a separate "commit-time recheck"
  unnecessary (see design note above)
- Catalog metadata uses `serde_json` (unlike per-row on-disk data, which is
  hand-rolled) — schema changes are infrequent control-plane operations, not
  the D9 "no serde" hot path; table rows themselves are hand-rolled tag+value
  encoded, which *is* the hot path (see design note above)
- Table storage (`Heap`) is reconstructed fresh per SQL statement from the
  catalog's persisted `TableDef.pages` list, not cached long-lived on `Engine`
  — cheap (just a `Vec<PageId>` clone) and avoids a cache-invalidation story
  for M1's scope

---

## In progress

Nothing — M5 milestone fully closed out (all four checkpoints verified,
benchmarked, committed). M0-M5 are all DONE — every milestone on
CLAUDE.md's original roadmap has shipped. The only remaining known-and-
deferred work is the cross-domain "replaced stack" benchmark follow-up
(see Current status above); anything beyond that is unplanned and should
be raised with the user directly, not assumed.

---

## M1.a task breakdown (ordered — all complete)

1. ✅ Error variants (`error.rs`): `WriteConflict`, `SerializationFailure`,
   `TxnNotActive`, `TxnAlreadyFinished`, `NoVisibleVersion`, SQL/catalog
   placeholders for later checkpoints.
2. ✅ Tuple header 16→24 bytes + `FORMAT_VERSION` 1→2 (`page.rs`/`format.rs`).
3. ✅ Control file `catalog_root` field (`control.rs`).
4. ✅ WAL user-txn record types + `begin/commit/abort_user_txn` (`wal.rs`/`format.rs`).
5. ✅ MVCC visibility logic (`mvcc.rs`, new).
6. ✅ Transaction manager (`txn.rs`, new) — built together with heap rewrite
   (task 7) since they're tightly coupled; see design notes above.
7. ✅ Heap MVCC rewrite (`heap.rs`).
8. ✅ User-txn recovery undo pass (`recovery.rs`).
9. ✅ `on_read`/`on_write` seam (`concurrency_hooks.rs`, new), threaded
   through every `Heap` read/write path.
10. ✅ Crash tests P6/P7 (`tests/crash/main.rs`).
11. ✅ M1.a checkpoint verification: `Engine::begin/commit/abort` wired,
    71 unit tests + 8 crash tests green, clippy/fmt clean, release build OK.

**M1.a done when:** transactional `Engine::begin/commit/abort` works around
insert/get/update/delete ✅, RC vs RR visibility distinction verified by a
hand-written interleaved-transaction test ✅ (`repeatable_read_does_not_see_write_committed_after_begin`
in `lib.rs`), all tests green ✅.

## M1.b task breakdown (ordered — all complete)

1. ✅ Lock manager (`lockmgr.rs`, new): `RecordKind`/`RecordId`/`LockManager`,
   write-write only, no wait queue (D12).
2. ✅ Wired `try_acquire_write` into `Heap::update`/`delete`, before the
   mini-txn begins; `Engine`/`TransactionManager` now own/thread a
   `LockManager` alongside `pool`/`wal`/`heap`.
3. ✅ Investigated the planned "commit-time first-committer-wins recheck" and
   found it subsumed by lock-held-until-commit — documented as a design note
   rather than building redundant code; verified with 3 hand-written
   interleaved-transaction tests in `lib.rs`.
4. ✅ Crash test P9 (`tests/crash/main.rs`): crash mid-undo of an
   already-aborting transaction; recovery converges to fully-undone via the
   same idempotent pass built in M1.a task 8.
5. ✅ M1.b checkpoint verification: 80 unit tests + 9 crash tests green,
   clippy/fmt clean, release build OK.

**M1.b done when:** SI's abort-on-conflict path works end-to-end (a second
concurrent writer aborts immediately, no blocking) ✅, locks correctly
release on both commit and abort so a later writer can proceed ✅, crash
safety extended to the new abort/undo machinery (P9) ✅, all tests green ✅.

## M1.c task breakdown (ordered — all complete)

1. ✅ Catalog (`catalog.rs`, new): `ColumnDef`/`ColumnType`/`TableDef`/
   `Catalog`, `CatalogCtx` bundling persistence dependencies (clippy
   too-many-arguments), heap-backed-in-spirit but actually a single
   `serde_json` blob per change (simpler than reusing `Heap`'s MVCC path,
   which would've needed a "not MVCC-versioned" escape hatch anyway).
2. ✅ Added `sqlparser` (0.62.0) + `serde_json` + `serde` (with `derive`) to
   `Cargo.toml` via `cargo add`.
3. ✅ SQL parser (`sql/parser.rs`, new): wraps `sqlparser::Parser` with
   `GenericDialect`, converts its AST to `LogicalPlan`. Grammar subset:
   CREATE TABLE, INSERT (with/without column list), SELECT (star or named
   projection, AND-only WHERE), UPDATE, DELETE. Discovered `->`/`->>` bind
   *looser* than `=` under `GenericDialect`'s precedence table — the
   opposite of the initial assumption — so `data -> 'k' = 'v'` parses as
   `data -> ('k' = 'v')`; explicit parens required (documented in test
   comments and the module's own scope, not a bug to fix — SQL operator
   precedence is a dialect detail, not something to special-case).
4. ✅ Logical plan + RLS rewrite (`sql/logical.rs`, new): `LogicalPlan`/
   `Expr`/`Literal`/`CmpOp`, `apply_rls` (the entire RLS story, one
   AND-rewrite function).
5. ✅ JSON column type (already added to `catalog.rs` in task 1) +
   `Expr::JsonExtract`/`JsonExtractText` (`->`/`->>`) — parsed in task 3,
   evaluated in task 6's executor via `serde_json::Value` navigation.
6. ✅ Executor (`sql/executor.rs`, new) — no separate physical-plan IR (see
   design note above); row-at-a-time; hand-rolled row encoding; fixed a
   real latent bug in the same pass (`Heap` page-list persistence, see
   design note above).
7. ✅ Wired `Engine::execute_sql`/`set_rls_policy`; `Engine` now owns a
   `Catalog`, loaded via `Catalog::load` on every `open()`.
8. ✅ M1.c checkpoint verification: 112 unit tests + 9 crash tests green,
   clippy/fmt clean, release build OK.

**M1.c done when:** `CREATE TABLE` → `INSERT` → `SELECT ... WHERE` →
`UPDATE ... WHERE` → re-`SELECT` → `DELETE ... WHERE` → re-`SELECT`
round-trips correctly through the SQL API ✅ (`execute_sql_full_round_trip`
in `lib.rs`), including a JSON column with `->`/`->>` ✅
(`json_column_round_trip_and_extract` in `sql/executor.rs`), RLS filters
rows end-to-end ✅ (`rls_policy_filters_rows` in `lib.rs`), data survives
reopen via the catalog's persisted page list ✅ (`sql_survives_reopen`), all
tests green ✅.

## M1.d task breakdown (ordered — all complete)

1. ✅ Combined crash+MVCC property test (`tests/crash/main.rs`, new): a
   self-contained LCG (no new dependency) drives random `BEGIN`/`INSERT`/
   `COMMIT`/`ROLLBACK` sequences across 6 seeds, crashing (just stopping)
   at a random point — sometimes mid-transaction with no commit/abort call
   at all, sometimes right after one finishes. Added `Hash` to `RowId`'s
   derive to track expected rows in a `Vec`. Passed on the first run.
2. ✅ Extended `benches/load.rs` with a `contention` benchmark group:
   interleaved transactions racing for one row, second aborts immediately
   (D12) and retries — measures the real cost of SI's conflict path, not
   just uncontended CRUD.
3. ✅ Ran the full benchmark suite (`--sample-size 10`, reduced from the
   default 100 to keep wall-clock reasonable given fsync-dominated cost)
   and recorded M1's metrics table in `PROGRESS.md`, including an M0
   comparison. **Found a real bug in the process** — see the read-only-txn
   fsync design note above — rather than just reporting the raw numbers.
4. ✅ M1.d checkpoint verification: 112 unit tests + 10 crash tests (P1–P9
   plus the new property test) green, clippy/fmt clean, release build OK.

**M1.d done when:** the combined crash+MVCC property test passes ✅, M1's
benchmark table is recorded with an honest M0 comparison ✅ (including
reporting, not hiding, the read-only-txn regression found along the way),
all tests green ✅ — closing out the M1 milestone as a whole.

## M2.a task breakdown (ordered — all complete)

1. ✅ `ColumnType::Vector(u32)` + `IndexKind{Hnsw,FullText}` +
   `ColumnDef.index: Option<IndexKind>` (`catalog.rs`). Mechanical fix-up of
   every existing `ColumnDef` literal across `catalog.rs`/`sql/*.rs` tests
   to add the new field.
2. ✅ Vector row encoding tag 5 (`sql/executor.rs`): `coerce_value`,
   `encode_row`, `decode_row` all handle `Literal::Vector`/
   `ColumnType::Vector(n)`, dimension-checked, no panics.
3. ✅ `Literal::Vector(Vec<f32>)` (`sql/logical.rs`).
4. ✅ Parser support (`sql/parser.rs`): `VECTOR(n)` via `DataType::Custom`
   fallback, `[..]` array literals via `SqlExpr::Array` → `f32` elements.
5. ✅ M2.a checkpoint verification: end-to-end SQL round-trip
   (`execute_sql_vector_round_trip`, `execute_sql_vector_dimension_mismatch_rejected`
   in `lib.rs`) plus parser/executor unit tests; 121 unit tests + 10 crash
   tests green, clippy/fmt clean.

**M2.a done when:** `CREATE TABLE t (id INT, embedding VECTOR(4))` →
`INSERT ... VALUES (1, [0.1, 0.2, 0.3, 0.4])` → `SELECT` round-trips
correctly through the actual SQL layer ✅, dimension mismatches rejected
with a clear `DbError::SqlPlan` ✅, all tests green ✅. No index/worker yet
— that's M2.b.

## M2.b task breakdown (ordered — all complete)

1. ✅ `src/vector.rs` (new): `VectorIndex` wrapper around `instant-distance`.
   Corrected the plan's "native incremental insertion" assumption after
   checking the vendored source (see design note above) — buffers points,
   rebuilds the HNSW graph on every `upsert`/`remove`.
2. ✅ `src/index_worker.rs` (new): the engine's first background thread.
   `IndexMsg{Upsert,MarkReady,Shutdown}`, `IndexedColumn::Vector`,
   `SecondaryIndex::Vector` (only variant so far — `FullText` lands in
   M2.c), `IndexStatus{Building{rows_done},Ready}`, `IndexHandle` with a
   bounded (5s) `shutdown()`. Worker owns only
   `Arc<RwLock<HashMap<(table,column), IndexEntry>>>`, never
   `BufferPool`/`Wal`/`Heap`.
3. ✅ Rebuild-on-open (`lib.rs::rebuild_vector_indexes`): runs on the
   foreground thread via an ordinary begin/scan/commit read-only
   transaction (same MVCC path as `SELECT`), pipes results through the same
   channel live upserts use.
4. ✅ Live upserts (`sql/executor.rs::send_vector_upserts`): checked once
   per inserted/updated row via `ColumnDef.index`, zero cost for
   non-indexed tables.
5. ✅ `Arc<RwLock<>>` shared index access — built directly into
   `index_worker.rs`'s `SharedIndexes` type from the start (not a
   follow-up), ready for M2.d's `NEAR` queries to take a read lock.
6. ✅ `Engine` gains an `index_worker: IndexHandle` field + `Drop` impl
   calling `shutdown()`.
7. ✅ Added `Catalog::set_column_index`/`Engine::set_column_index` ahead of
   its originally-planned M2.c slot, narrowly justified as the same
   primitive `CREATE INDEX` will call internally (see design note above) —
   needed now so M2.b's own tests could prove the worker pipeline
   end-to-end without waiting for the full `CREATE INDEX` SQL surface.
8. ✅ Tests: `index_worker.rs`'s own unit tests (send/status/shutdown in
   isolation) + three `lib.rs` integration tests exercising the real
   `Engine`: live-insert-enqueues-upsert, reopen-rebuilds-from-committed-rows,
   and drop-doesn't-hang.
9. ✅ M2.b checkpoint verification: 131 unit tests + 10 crash tests green,
   clippy/fmt clean, release build OK.

**M2.b done when:** the worker spawns on `Engine::open` ✅, correctly
rebuilds a `VectorIndex` from committed rows ✅
(`reopen_rebuilds_index_from_committed_rows`), live inserts/updates enqueue
upsert messages ✅ (`live_insert_into_indexed_column_enqueues_upsert`),
shutdown is clean and tested ✅ (`engine_drop_shuts_down_worker_without_hanging`),
`IndexStatus` reports `Building`/`Ready` correctly ✅, all tests green ✅.

## M2.c task breakdown (ordered — all complete)

1. ✅ `src/fulltext.rs` (new): `InvertedIndex` — whitespace+lowercase
   tokenization, `HashMap<String, Vec<RowId>>` postings, AND-only
   multi-term intersection search, `upsert`/`remove` mirroring
   `VectorIndex`'s API shape.
2. ✅ Generalized `index_worker.rs`: `SecondaryIndex::FullText(InvertedIndex)`,
   `IndexedColumn::Text{column,data}`, extended `worker_loop`'s match arm —
   confirmed the message/status plumbing needed zero shape changes, exactly
   as M2.b's design note predicted.
3. ✅ `LogicalPlan::CreateIndex{table,column,kind}` (`sql/logical.rs`) +
   parser support (`sql/parser.rs`) for `CREATE INDEX ... ON t USING
   HNSW|FULLTEXT (column)`. Found and documented a real grammar detail:
   `USING` must precede the column list, not follow it (see design note
   above) — caught before shipping broken tests, not after.
4. ✅ `exec_create_index` (`sql/executor.rs`): validates column-type
   compatibility, persists via `Catalog::set_column_index` (built ahead of
   schedule in M2.b), immediately backfills every committed row through the
   worker channel, sends `MarkReady`. Factored `build_indexed_columns` out
   as the one shared column-type-to-`IndexedColumn` mapping, used by both
   live upserts and every backfill path.
5. ✅ **Found and fixed a latent gap while building this**: `lib.rs`'s
   rebuild-on-open only ever scanned `IndexKind::Hnsw` columns — a
   `FullText`-indexed table would have silently lost its index on every
   reopen. Generalized (`rebuild_vector_indexes` → `rebuild_secondary_indexes`)
   to scan any indexed column, using the same shared `build_indexed_columns`
   helper from task 4.
6. ✅ Tests: executor-level validation (rejects `Hnsw` on `Text`, rejects
   `FullText` on `Vector`, rejects unknown column, persists correctly for
   both valid combinations) + two `lib.rs` integration tests through the
   real `Engine`: immediate-backfill-and-queryable, and
   type-mismatch-rejected-via-SQL.
7. ✅ M2.c checkpoint verification: 148 unit tests + 10 crash tests green,
   clippy/fmt clean, release build OK.

**M2.c done when:** `CREATE INDEX ... USING FULLTEXT` builds and maintains
an `InvertedIndex` via the shared worker ✅, term search returns correct
intersections ✅, tokenization tests pass ✅, `CREATE INDEX` validation
rejects type-kind mismatches ✅, all tests green ✅.

## M2.d task breakdown (ordered — all complete)

1. ✅ `Expr::Near{column,query,k}` (`sql/logical.rs`) + parser support
   (`sql/parser.rs`): `NEAR(...)` parses unmodified as `SqlExpr::Function`,
   confirmed against `sqlparser`'s AST before writing the conversion code.
2. ✅ `exec_select_near` (`sql/executor.rs`): over-fetch-then-filter
   execution — validates the column is `Hnsw`-indexed on a `Vector` column,
   over-fetches from `VectorIndex::search`, resolves candidates via
   `Heap::get` + the ordinary MVCC snapshot, re-runs the full predicate
   through `predicate_matches`. `eval_expr`'s new `Expr::Near` arm always
   returns `true` on recheck (proximity already established).
3. ✅ **Found and fixed a real bug while wiring this up**: `MarkReady` on a
   column that had never received an `Upsert` (e.g. `CREATE INDEX` on an
   empty table) silently no-opped, permanently stranding the column in
   `Building` once a later live insert finally arrived. Fixed by having
   `MarkReady` carry `IndexKind` and create an already-`Ready` empty entry
   when none exists yet — caught by two failing `lib.rs` NEAR tests before
   it could ship, then covered by a dedicated regression test in
   `index_worker.rs`.
4. ✅ `tests/index_rebuild.rs` (new): engine-restart rebuild correctness for
   both index kinds, `NEAR`-while-`Building` returns a valid (possibly
   partial) result set without erroring.
5. ✅ `tests/vector_mvcc.rs` (new) — **the single most important test in
   M2**: inserts a row, deterministically polls (via the inserting
   transaction's own self-visible `NEAR` query, not a timing guess) until
   the worker has demonstrably indexed it, aborts instead of committing,
   then proves a fresh transaction's `NEAR` query never returns that row.
6. ✅ `benches/vector.rs` (new) + a real, no-mocking Postgres 18 + pgvector
   0.8.4 comparison run locally (`brew install pgvector`, isolated
   `unidb_bench` database, dropped after recording numbers). Recorded
   honestly in `PROGRESS.md`, including where unidb is far behind and why
   (pre-existing per-statement fsync cost from M1, `instant-distance`'s
   full-rebuild-per-upsert cost) — not flattered.
7. ✅ M2.d / M2 milestone checkpoint verification: 158 unit + 10 crash + 3
   `index_rebuild` + 1 `vector_mvcc` tests (172 total) green, clippy/fmt
   clean, release build OK.

**M2.d done when:** `SELECT ... WHERE NEAR(col, [...], k)` returns
MVCC-correct, RLS-respecting results end-to-end ✅; the rollback-correctness
test passes ✅; rebuild-after-restart and mid-rebuild-staleness tests pass
✅; M2's benchmark table is recorded with the Postgres+pgvector comparison
✅; all tests green ✅ — closing out the M2 milestone as a whole.

---

## M3.a task breakdown (ordered — all complete)

1. ✅ `src/graph/mod.rs` (new) + `pub mod graph;` in `lib.rs`.
2. ✅ `src/graph/edges.rs` (new): `EDGES_TABLE`, `edges_table_def()`,
   `edge_row()`, `ensure_edges_table()` (idempotent, called from
   `Engine::open()`). Reuses `sql::executor::encode_row`/`decode_row`
   verbatim — no new tag byte.
3. ✅ `src/graph/index.rs` (new): `EdgeIndex` (plain `HashMap<i64,
   Vec<RowId>>`, synchronous — no background worker, unlike M2) +
   `resolve_candidates_batched` (built batched from the start, per the
   plan, rather than shipping a naive version first).
4. ✅ `Engine::create_edge`/`delete_edge` (`lib.rs`): reconstruct their own
   `Heap::from_pages` against `__edges__`'s catalog page list — deliberately
   not `self.heap`, which has no table concept.
5. ✅ `rebuild_edge_index` (`lib.rs`): synchronous rebuild-on-open, mirroring
   `rebuild_secondary_indexes`'s shape but with no channel/worker/status.
6. ✅ `Engine::edges_from` (`lib.rs`): MVCC-filtered traversal via
   `resolve_candidates_batched`.
7. ✅ Tests: idempotent table creation, create/delete/traversal round-trip,
   index rebuild-on-reopen, empty-`from_id` returns empty,
   `__edges__` ordinary-SQL-queryable.
8. ✅ M3.a checkpoint verification: 168 unit + 10 crash + 3 `index_rebuild`
   + 1 `vector_mvcc` (182 total) green, clippy/fmt clean, release build OK.

**M3.a done when:** `create_edge`/`delete_edge`/`edges_from` round-trip
correctly ✅, the index rebuilds on reopen from committed rows ✅, deleted
edges are absent from both the index and traversal results ✅, all tests
green ✅.

## M3.b task breakdown (ordered — all complete)

1. ✅ `tests/graph_locking.rs` (new): confirmed per-edge locking needs zero
   new code — concurrent edge deletes conflict immediately (D12), an edge
   lock and an unrelated table's row lock never collide, locks release
   correctly on both commit and abort. See the design note above for the
   `WriteConflict`-shares-one-shape gotcha found while writing these.
2. ✅ `MEMORY.md` design note (see above) on why no `RecordKind::GraphEdge`
   variant was needed — `RecordId::row`'s lock key is already globally
   unique across every table.
3. ✅ `benches/graph.rs` (new): `adjacency_scan` before/after benchmark
   (naive vs. `resolve_candidates_batched`) — a real ~9.3–9.7x win at
   1,000/10,000-edge hot hubs, not assumed (see design note above);
   `edge_insert` uncontended throughput.
4. ✅ M3.b checkpoint verification: 168 unit + 10 crash + 4 `graph_locking`
   + 3 `index_rebuild` + 1 `vector_mvcc` (186 total) green, clippy/fmt
   clean, release build OK.

**M3.b done when:** locking tests pass proving zero new locking code was
needed ✅, `edges_from` is wired to the batched resolver ✅ (done in M3.a),
a recorded before/after benchmark number exists for a hot-hub workload ✅.

## M3.c task breakdown (ordered — all complete)

1. ✅ `src/graph/logical.rs` (new): `CypherQuery{from_var,to_var,edge_type,
   predicate,returns}`, `ReturnItem{FromVar,ToVar,EdgeColumn}`.
2. ✅ `src/graph/parser.rs` (new): hand-rolled tokenizer + recursive-descent
   parser for `MATCH (a)-[:TYPE]->(b) WHERE <predicate> RETURN <items>` —
   no external crate (see M3's planning notes: no viable Cypher-parsing
   crate exists on crates.io). `-[]->` (empty brackets) matches any edge
   type. `a.x`/`b.x` property access rejected with a clear error at parse
   time, enforcing the opaque-node-IDs decision at the boundary rather than
   leaving it to the executor to notice.
3. ✅ `predicate_matches`/`eval_expr` promoted from private to `pub(crate)`
   in `sql/executor.rs` — the one deliberate cross-module touch needed for
   reuse (see design note above).
4. ✅ `src/graph/executor.rs` (new): `execute(query, ctx, edge_index)`
   reuses `sql::executor::ExecCtx`/`ExecResult`/`predicate_matches`
   verbatim; `edge_index` passed as an extra argument rather than a new
   `ExecCtx` field (see design note above for why). `find_from_id_eq`
   (mirrors `sql/executor.rs`'s `find_near`) detects the index fast-path
   opportunity; falls back to a full `__edges__` scan otherwise. The
   `:TYPE` pattern filter and the `WHERE` predicate are AND'd into one
   `full_predicate` before either path runs.
5. ✅ `Engine::execute_cypher` (`lib.rs`): mirrors `execute_sql`'s exact
   `ExecCtx` construction shape.
6. ✅ Tests: parser (valid single-hop, empty-bracket wildcard type, AND'd
   WHERE, edge-column RETURN, property-access rejection, case-insensitive
   keywords, missing RETURN rejected) + `lib.rs` integration tests via the
   real `Engine::execute_cypher` (index fast path, edge-type filtering,
   full-scan fallback when no `from_id` equality is present, `RETURN
   type, props`, property-access rejection end-to-end).
7. ✅ M3.c checkpoint verification: 182 unit + 10 crash + 4
   `graph_locking` + 3 `index_rebuild` + 1 `vector_mvcc` (200 total)
   green, clippy/fmt clean, release build OK.

**M3.c done when:** a `MATCH`/`WHERE`/`RETURN` query round-trips
end-to-end through `Engine::execute_cypher` ✅, reuses `predicate_matches`/
`eval_expr` with no duplicate evaluator ✅, the equality fast path is
proven to hit the edge index ✅ (and the full-scan fallback is proven to
work when it doesn't apply ✅), all tests green ✅.

## M3.d task breakdown (ordered — all complete)

1. ✅ `tests/graph_rebuild.rs` (new): engine restart rebuilds the edge
   index and traversal/Cypher queries both still work — no polling needed,
   unlike M2's async-worker-backed indexes; deletes correctly reflected
   after reopen.
2. ✅ `tests/graph_mvcc.rs` (new) — the single most important test in M3:
   create an edge, confirm self-visibility via `edges_from` (proving the
   index really has the stale-entry-to-be), abort, then prove both
   `edges_from` and an equivalent Cypher query never return it from a
   fresh transaction. See design note above.
3. ✅ `benches/graph.rs` extended with a real, non-mocked Postgres
   comparison (isolated `unidb_graph_bench` database, dropped after
   recording numbers): INSERT throughput and indexed adjacency-scan
   latency. Recorded honestly in `PROGRESS.md` — including the strong,
   unexpected-in-a-good-way result that batch-latch brings adjacency-scan
   performance within ~1.6x of Postgres. See design note above.
4. ✅ `PROGRESS.md`'s `## M3 — Graph [DONE]` entry + this file's closeout.
5. ✅ M3.d / M3 milestone checkpoint verification: 182 unit + 10 crash + 4
   `graph_locking` + 3 `graph_rebuild` + 2 `graph_mvcc` + 3
   `index_rebuild` + 1 `vector_mvcc` (205 total) green, clippy/fmt clean,
   release build OK.

**M3.d done when:** both new test files pass ✅, `benches/graph.rs` runs
with a recorded Postgres-adjacency-table comparison in `PROGRESS.md` ✅,
docs updated ✅, all tests green ✅ — closing out M3 as a whole.

---

## M4.a task breakdown (ordered — all complete)

1. ✅ `src/queue/mod.rs` (new): `EVENTS_TABLE`/`CONSUMERS_TABLE` consts,
   `events_table_def()`/`consumers_table_def()`, `Event` struct,
   `event_row()`/`consumer_row()`, `ensure_queue_tables()` (mirrors
   `graph::edges::ensure_edges_table` line-for-line).
2. ✅ `src/queue/payload.rs` (new): `row_to_json` — the one place a
   `Vec<Literal>` becomes a `serde_json::Value`, unit-tested per `Literal`
   variant plus a mixed-column row.
3. ✅ `TableDef.events_enabled: bool` (`#[serde(default)]`, mirroring
   `ColumnDef.index`'s M2.a introduction) + `Catalog::set_events_enabled`.
4. ✅ `Engine::enable_events` (rejects `__events__`/`__consumers__` as
   targets) + `queue::ensure_queue_tables` called from `Engine::open()`.
5. ✅ `next_event_seq: u64` field on `Engine` + `derive_next_event_seq`
   (copies `rebuild_edge_index`'s exact begin/scan/commit template).
6. ✅ `sql::executor::send_event_capture` + wired into `exec_insert`/
   `exec_update`/`exec_delete` (delete's payload captured *before*
   `heap.delete` runs). See design notes above for the inline-not-hook
   decision and the `ExecCtx`-field deviation from the original plan.
7. ✅ Tests: opt-in gating, correct per-op tagging + JSON payloads,
   abort-visibility (self-visible then invisible to a fresh transaction —
   written now, not deferred to M4.d), `seq` resumption across reopen.

**M4.a done when:** an events-enabled table's INSERT/UPDATE/DELETE each
produce exactly one correctly-tagged, JSON-payloaded row in `__events__`
under the same xid ✅; a non-events-enabled table produces zero
`__events__` rows ✅; an aborted transaction's event row is provably
invisible to a fresh transaction ✅; `seq` derivation correct fresh and
after reopen ✅; all tests green ✅.

## M4.b task breakdown (ordered — all complete)

1. ✅ `queue::find_consumer_offset` — scans `__consumers__` for a durable
   offset; `None` means never-acked, treated as offset 0 by the caller,
   purely in-memory.
2. ✅ `Engine::poll_events` — pure read, ascending by `seq`, truncated to
   `limit`; never writes to `__consumers__` even for an unregistered
   consumer (that only happens on first `ack_events`).
3. ✅ `Engine::ack_events` — the only write path to `__consumers__`;
   `heap.insert` (first ack, durable auto-registration) or `heap.update`
   (subsequent acks), using the same two-`record_undo`-call shape
   `exec_update` already uses.
4. ✅ Tests: no-auto-advance on poll, ack advancing what a fresh
   transaction sees, offset persistence across reopen, independent
   consumers not interfering, unregistered polls not writing.

**M4.b done when:** `poll_events` never advances state on its own ✅;
`ack_events` durably advances the offset and that survives an `Engine`
reopen ✅; independent consumers demonstrably don't share or clobber
state ✅; all tests green ✅.

## M4.c task breakdown (ordered — all complete)

1. ✅ `Engine::vacuum_events` — no-op with zero registered consumers
   (a not-yet-registered consumer might need full history); otherwise
   reclaims every `__events__` row with `seq <= min(all registered
   consumers' offsets)` via the ordinary `lock_mgr`/`record_undo` path.
   Confirmed by reading `Engine::checkpoint`'s actual call site that it
   is never invoked automatically.
2. ✅ `tests/queue_vacuum.rs` (new): the milestone's central-claim proof,
   `wal_truncation_is_unaffected_by_consumer_lag` (a never-acking consumer
   doesn't block five consecutive `checkpoint()` calls), plus
   `slow_consumer_survives_vacuum_fast_consumer_does_not_block_it`,
   `vacuum_is_noop_with_zero_registered_consumers`,
   `vacuum_reclaims_up_to_min_offset_when_consumers_advance`.

**M4.c done when:** `vacuum_events` is a no-op with zero consumers ✅,
correctly bounds reclaim to `min(offsets)` across multiple consumers ✅, a
slow consumer's un-acked events demonstrably survive vacuum without
blocking a fast consumer's independent progress ✅, WAL truncation is
proven via a concrete test to proceed unaffected by consumer lag ✅,
`vacuum_events` confirmed never called automatically ✅, all tests
green ✅.

## M4.d task breakdown (ordered — all complete)

1. ✅ `tests/queue_mvcc.rs` (new) — self-visibility then invisibility for
   an aborted event insert; a second test proving an aborted `ack_events`
   call does not durably advance the offset a fresh transaction sees.
2. ✅ New two-table crash-recovery test in `tests/crash/main.rs` (no new
   P-number): `incomplete_user_txn_leaves_no_trace_across_two_tables` — a
   transaction that inserts into both a triggering table and `__events__`,
   then never reaches `WAL_TXN_COMMIT`, leaves no trace in *either* table
   after reopen. First crash test to span two tables in one incomplete
   user transaction.
3. ✅ `benches/queue.rs` (new): event-capture overhead (events on vs.
   off), `poll_events` latency vs. `__events__` size, `vacuum_events` cost
   vs. reclaimed-row count, plus a real, non-mocked Postgres SKIP LOCKED
   comparison (isolated `unidb_queue_bench` database, dropped after
   recording numbers).
4. ✅ `PROGRESS.md`'s `## M4 — Event queue [DONE]` entry + this file's
   closeout.
5. ✅ M4.d / M4 milestone checkpoint verification: 203 unit + 11 crash + 4
   `graph_locking` + 3 `graph_rebuild` + 2 `graph_mvcc` + 3
   `index_rebuild` + 1 `vector_mvcc` + 4 `queue_vacuum` + 2 `queue_mvcc`
   (233 total) green, clippy/fmt clean, release build OK.

**M4.d done when:** the aborted-event MVCC test passes (including the
`ack_events`-abort case) ✅; the two-table crash-recovery test passes ✅;
the queue-scoped benchmark table (with the Postgres SKIP LOCKED
comparison) is recorded ✅; `PROGRESS.md`/`MEMORY.md` closeout complete ✅;
all tests green ✅ — closing out M4 as a whole.

---

## M5.a task breakdown (ordered — all complete)

1. ✅ Compile-time `Engine: Send` assertion near the `Engine` struct in
   `lib.rs` — turns "believed true" into "compiler-enforced" ahead of
   moving `Engine` into a dedicated writer thread.
2. ✅ Crate-level `//!` doc comment on `lib.rs` (previously had none) +
   transaction-boundary doc comments on `insert`/`get`/`delete`/
   `checkpoint`/`begin_with_isolation`/`commit`/`abort`.
3. ✅ `unwrap`/`expect` audit — confirmed every non-test occurrence is
   either infallible-by-construction (bounds-checked slice-to-array
   conversions), an internal invariant proven by preceding code, or an
   already-accepted RwLock-poisoning/thread-spawn-failure exception. See
   design note above.
4. ✅ `src/server/` (`engine_handle.rs`, `error.rs`, `mod.rs`) behind a new
   `server` Cargo feature; `EngineHandle` mirrors `index_worker.rs`'s
   spawn/channel/bounded-shutdown shape exactly.

**M5.a done when:** `Engine: Send` compiler-verified ✅; `EngineHandle`
round-trips a request and shuts down within its bound, with a fresh
`Engine::open` succeeding immediately after ✅; default `cargo build`/
`cargo test` unaffected, `cargo tree --no-default-features --edges normal`
empty of tokio ✅; clippy/fmt clean both with and without `--features
server` ✅.

## M5.b task breakdown (ordered — all complete)

1. ✅ axum/tokio brought in behind `server`; `src/server/dto.rs`,
   `handlers.rs`, `router.rs`, `src/bin/unidb-server.rs`.
2. ✅ Every mutating route wraps one `begin -> execute -> commit-or-abort`
   cycle; `/sql`/`/cypher` get atomic multi-statement transactions over
   HTTP for free via `execute_sql`'s existing `;`-separated-string support.
3. ✅ `RowId`/`Edge`/`Event`/`IndexStatus` gained plain `serde::Serialize`
   derives (unconditional, not feature-gated — `serde` is already a core
   dependency via `Literal`). Deliberately did **not** derive `Serialize`
   on `Literal`/`ExecResult` themselves — see design note above;
   `server::dto::literal_to_json`/`exec_result_to_json` do the REST-facing
   conversion explicitly instead.
4. ✅ Manually smoke-tested end-to-end against a running `unidb-server`.

**M5.b done when:** every route serves against real `curl`/`reqwest`
calls ✅; a multi-statement `/sql` body's failing last statement leaves no
prior statement's row data committed ✅; default build still excludes
tokio/axum entirely ✅; clippy/fmt clean both ways ✅.

## M5.c task breakdown (ordered — all complete)

1. ✅ `src/server/auth.rs` — verify-only HS256 JWT via `jsonwebtoken`'s
   `aws_lc_rs` backend, secret from `UNIDB_JWT_SECRET`. No login endpoint,
   no user database, no session state.
2. ✅ `src/server/sse.rs` — `GET /events/subscribe`, an `async-stream` loop
   polling `poll_events` on an interval, explicitly documented as "server
   polls, pushes to client," not WAL-level push.
3. ✅ `POST /tables/{table}/events` (new — M5.b never exposed
   `Engine::enable_events` over HTTP).
4. ✅ `GET /metrics` via `axum-prometheus`'s `PrometheusMetricLayer`;
   `router.rs` restructured into a `protected` sub-router (JWT-wrapped)
   merged with a `public` one (`/metrics` only, no auth layer).
5. ✅ Manually verified end-to-end: auth rejection matrix, SSE delivery +
   redelivery-until-ack, custom + auto-instrumented Prometheus metrics.

**M5.c done when:** missing/malformed tokens rejected with 401 on data
routes ✅; a valid token succeeds ✅; `/metrics` needs no auth and returns
real Prometheus text ✅; SSE delivers a committed mutation within one poll
interval and stops redelivering after ack ✅.

## M5.d task breakdown (ordered — all complete)

1. ✅ `tests/server_common/mod.rs` (new, shared scaffolding, not its own
   test binary) — `TestServer` + JWT token helpers. Required restructuring
   `build_router` to accept an already-obtained `PrometheusMetricLayer`/
   `PrometheusHandle` pair as an argument rather than calling `pair()`
   internally: that call installs a process-global `metrics` recorder,
   and multiple test functions spawning independent servers within one
   test binary process would otherwise panic on the second call.
   Production code (`unidb-server`'s `main()`) is unaffected — it already
   only calls `build_router` once.
2. ✅ `tests/server_crud.rs`, `server_sql.rs` (the central transaction-
   model proof + `Literal::Json`-as-real-JSON proof), `server_cypher.rs`,
   `server_graph.rs`, `server_auth.rs` (5-case matrix), `server_events.rs`
   (SSE delivery + ack-prevents-replay), `server_shutdown.rs` (graceful
   shutdown drains in-flight requests, preserves committed data — no new
   crash-injection P-number needed), `server_metrics.rs` — each gated via
   its own `[[test]] required-features = ["server"]` entry in Cargo.toml,
   mirroring the `unidb-server` binary's own gating.
3. ✅ `benches/server.rs` (`[[bench]] required-features = ["server"]`):
   direct-vs-HTTP insert overhead, JWT verification cost, SSE polling
   overhead at 1/10/50 subscribers, concurrent `POST /sql` throughput at
   1/10/50 clients. Server-overhead-focused only, per the confirmed
   decision not to fold the deferred cross-domain benchmark into M5.
4. ✅ `PROGRESS.md`'s `## M5 — API / server [DONE]` entry + this file's
   closeout.
5. ✅ M5.d / M5 milestone checkpoint verification: 205 unit (208 with
   `--features server`) + 11 crash + 4 `graph_locking` + 3 `graph_rebuild`
   + 2 `graph_mvcc` + 3 `index_rebuild` + 1 `vector_mvcc` + 4
   `queue_vacuum` + 2 `queue_mvcc` + 25 new `server_*` tests, all green
   both with and without `--features server`; clippy/fmt clean; `cargo
   tree --no-default-features --edges normal` confirmed empty of tokio/
   axum/jsonwebtoken (the plain `cargo tree --no-default-features`,
   *without* `--edges normal`, now shows them since they're dev-
   dependencies for the test suite — a testing-methodology correction
   worth recording, not a regression of the "engine stays sync" claim,
   which is about the production library/binary build graph only).

**M5.d done when:** every test file green including the shutdown-safety
and full 5-case auth matrix ✅; `cargo build`/`test`/`clippy --all-targets`
pass both with and without `--features server` ✅; `benches/server.rs`
numbers recorded in `PROGRESS.md` ✅; `PROGRESS.md`/`MEMORY.md` closeout
complete ✅ — closing out M5 as a whole, and with it, every milestone on
CLAUDE.md's original roadmap (M0-M5).

### Design note: M6 B-Tree — a real query-planning addition, not just a new IndexKind variant

Adding `HNSW`/`FullText` in M2 only ever needed a new `IndexKind` variant
plus a new `IndexedColumn`/`SecondaryIndex` case — every call site
(`send_index_upserts`, rebuild-on-open, `CREATE INDEX` backfill) was
already index-kind-agnostic. `BTree` needed all of that *plus* new logic
in `exec_select` itself, because `exec_select` previously had no concept
of "should I consult an index instead of scanning" at all — `NEAR` was
the only predicate ever routed to an index, and only because `NEAR` is an
explicit SQL operator with no non-indexed execution strategy. `BTree`
acceleration is invisible at the SQL surface (an ordinary `WHERE id = 5`)
so `find_indexable_btree_predicate`/`try_exec_select_btree` had to be
built as a genuine (if narrow) query-planning step: detect an indexable
top-level/AND'd `Column <op> Literal` comparison, check the column's index
status, and only then divert from the full-scan path — falling straight
back to it on any doubt (index missing, still `Building`, or the literal
isn't orderable).

**The `IndexStatus::Ready` gate matters more here than it did for `NEAR`.**
`NEAR`'s top-k is inherently approximate — returning fewer than `k`
results while a backfill races the query is expected and already
documented. An equality/range query has no such slack: trusting a
`Building` `BTreeIndex` (which has only indexed *some* rows so far) would
silently return an incomplete, *wrong* result set. `try_exec_select_btree`
therefore only uses the index once `Ready`, proven directly by
`btree_select_before_index_ready_still_returns_correct_full_result`
(inserts 50 rows without waiting for `Ready`, asserts the query still
finds the exact row via full-scan fallback).

**A `sqlparser` gotcha worth remembering:** a pre-existing test asserted
`CREATE INDEX ... USING BTREE (id)` was unsupported (it was used as the
"this should fail" example when the M2 plan wrote `rejects_create_index_
with_unsupported_using`). Implementing `IndexKind::BTree` broke that test
immediately — not because of a bug, but because `sqlparser`'s own
`ast::IndexType` enum already has a **native** `BTree` variant (`BTREE`
is a real, common index type name across Postgres/MySQL), unlike `HNSW`/
`FULLTEXT` which arrive as `IndexType::Custom`. The fix was matching
`Some(IndexType::BTree)` directly rather than `Custom(ident) if ident..
eq_ignore_ascii_case("btree")` — worth checking `sqlparser`'s `IndexType`
enum before assuming a new index-type keyword needs the `Custom` fallback
path M2's `HNSW`/`FULLTEXT` needed.

**A genuine, unrelated discovery made while writing `benches/btree.rs`:**
setting up two 100,000-row tables in one engine hit `DbError::
BufferPoolFull`, even after switching the benchmark's setup from one
giant transaction to one commit per 500-row batch (per-transaction pinned
pages were the first suspect, given the fixed 256-frame `POOL_CAPACITY`
in `lib.rs` — but per-batch commits alone didn't fully resolve it). This
points at a heap/FSM (free-space-map) page-allocation interaction that
grows pinned-page pressure as a table's *total* page count grows into the
hundreds, independent of any single transaction's size — not investigated
further (out of M6's scope), but real and worth a dedicated look before
any future benchmark or workload pushes a single table past roughly
10,000-50,000 rows in one engine. See `PROGRESS.md`'s M6 entry and the
"Known issues" list below.

### Design note: M7 CSR — debouncing a worker that previously applied every message immediately

`index_worker.rs`'s `worker_loop` had been a straightforward `for msg in
rx { match msg { ... } }` since M2.b — every message (Vector/Text/Ordered
upserts, MarkReady, Shutdown) applied immediately and completely, one at a
time. CSR's debounce requirement (a user-approved decision: coalesce a
burst of edge writes into one rebuild pass, rather than repeating HNSW's
"rebuild on every single upsert" mistake) needed a genuine restructure,
not just a new match arm: the loop was split into `apply_msg` (apply one
message; for CSR, just `stage` the edge and record its key as dirty — no
rebuild) and an explicit drain phase using `try_recv()` in a tight loop
after every `recv()`, only calling `rebuild_dirty` once the channel is
momentarily empty. This is a purely additive change in behavior — every
non-CSR message still applies exactly as before, immediately, no
debouncing — verified by the fact that zero existing `index_worker.rs`
tests needed changes, only new ones were added.

**How the debounce is actually proven, not just asserted**: `CsrIndex`
gained a test-only `rebuild_count()` counter. `index_worker.rs`'s
`burst_of_edge_upserts_coalesces_into_far_fewer_rebuilds_than_messages`
sends 200 `Upsert` messages back-to-back (no gap for the worker to drain
between sends), waits for `Ready`, then asserts `rebuild_count() < 200` —
deliberately not asserting exactly 1, since the sender and worker threads
race in ways a test can't fully pin down (the worker might wake and start
draining before all 200 sends complete, producing 2-3 rebuild passes
instead of 1). "Meaningfully less than N" is the honest, provable claim;
"exactly 1" would be an unprovable, occasionally-flaky one.

**The `EdgeIndex`-vs-CSR selection question was worked through explicitly
during planning — and the conclusion was wrong, corrected during M8 merge
verification.** Originally: `graph::index::graph_candidates` preferred CSR
whenever `Ready`, falling back to `EdgeIndex` (always current, zero lag)
otherwise — no "only use CSR above N candidates" heuristic. The reasoning
at the time: CSR's async rebuild lag can only ever cause a *missed*
very-recent edge (a false negative, since the edge hasn't been
staged-and-rebuilt into CSR yet), never a phantom one returned that
shouldn't be — every candidate from either index still goes through
`resolve_candidates_batched`'s MVCC re-validation before ever reaching a
caller. That's exactly the same staleness class HNSW/FullText/BTree
already have once `Ready`.

**What that reasoning missed**: it correctly rules out phantom edges, but
doesn't rule out the specific case of the *current transaction's own
just-created edge*. `edges_from` had always given self-visibility —
`create_edge` followed immediately by `edges_from` in the same
transaction (or even a later one, once committed) reliably saw the edge,
because `EdgeIndex` is synchronous. CSR's rebuild is debounced/async, so
`Ready` (true almost instantly for a fresh/empty table) does not imply
"this specific edge, written a moment ago, has been staged and
rebuilt-in." Preferring CSR broke that guarantee — not a "slight
staleness," a same-transaction miss. Found via `cargo test -p unidb --test
graph_mvcc aborted_edge_creation_never_surfaces_in_traversal` run
repeatedly (30/30 reproductions) in isolation from the rest of the test
suite; the same test passed reliably under `cargo test --workspace`,
because workspace-wide feature unification (`unidb-attach`'s
`dev-dependencies` pulling in `unidb`'s `server` feature) changed enough
about test binary composition/timing to mask the race. **Fix**:
`graph_candidates` was deleted; `edges_from`/`execute_cypher` now call
`EdgeIndex::candidates` directly and unconditionally, exactly as before
M7. `CsrIndex` remains built, kept warm on every live edge write
(`create_edge`), and rebuilt on open (`rebuild_csr_index`) — it's simply
not consulted by any query path right now. A correct fix (a staleness/
generation marker CSR could expose, proving it has incorporated every
write up to a specific point before a caller can trust it) is real future
work, not attempted here since this session's job was reverting a bug,
not designing new correctness machinery.

**Benchmark honesty note**: extending `benches/graph.rs`'s
`adjacency_scan` group with a `csr` variant found CSR performs at parity
with the already-fast `batched` (`EdgeIndex`) variant — no measurable win.
This was reported as the actual finding (`PROGRESS.md`'s M7 entry
explains why: the batched-resolve step already dominates cost for a
single-hop workload, and CSR's real advantage — contiguous, cache-friendly
adjacency for *multi-hop* traversal — has no way to show up until Cypher
supports multi-hop patterns, which it doesn't yet). Not massaged into a
flattering number; CLAUDE.md §6 explicitly wants exactly this kind of
plain reporting.

---

## Open questions / pending human input

- ~~**Decide: fix the read-only-transaction fsync now, or carry it into
  M2?**~~ **RESOLVED 2026-07-08** (branch `m9-group-commit`): fixed exactly
  as proposed — `TransactionManager::commit` now skips `commit_user_txn`
  (record + fsync) when `undo_log.is_empty()`. Treated as the deliberate
  commit-path change CLAUDE.md wanted, with the user's go-ahead. Point
  SELECT ~3.05 ms → 1.09 µs. Kept crossed off here so a future reader sees
  where it went. See `docs/backlog/group_commit_and_read_concurrency.md`.
- **Decide: is catalog DDL's lack of transactionality acceptable to carry
  into M2, or does it need addressing first?** (See below.)
- **The slow-consumer-vs-vacuum durability contract is now resolved (M4)** —
  see `PROGRESS.md`'s M4 entry and the M4.a design notes above. No longer
  an open question; removed from this list, kept as a crossed-off
  reference so a future reader doesn't wonder where it went.
- Still deferred-but-flagged for later milestones: filtered-HNSW vs
  over-fetch for RLS on `NEAR` (M2); SSI activation (post-M1, seam built in
  M1.a per D11, still all no-ops — M1.b's lock manager has no wait
  queue/deadlock detection, deliberately deferred to that future SSI
  effort); the full cross-domain "replaced stack" benchmark (now possible
  since M4 shipped, but explicitly deferred as a separate follow-up rather
  than folded into M4 — see Current status above).
- RC's EvalPlanQual-style re-evaluation path (D12, sequenced after SI) is
  designed but **still not implemented** even though M1.c's executor now
  exists (the thing it was waiting on) — UPDATE/DELETE conflicts propagate
  as `WriteConflict` regardless of isolation level. Not a blocker for M1's
  stated "prove SQL works" bar; flagged for whenever this becomes a real
  correctness gap in practice, since it's now unblocked and buildable.
- Catalog DDL is not MVCC-versioned/transactional (see design note above) —
  a `CREATE TABLE` inside a transaction that later aborts is **not** rolled
  back. This is a real, if narrow, correctness gap relative to "DDL is
  naturally transactional" from the original plan; flagged, not silently
  dropped.

---

## Known issues / tech debt

- **MVCC visibility anomaly under `UNIDB_CONCURRENT_SQL_WRITES` (item 11's
  default-OFF toggle) — OPEN, found 2026-07-11 during item-12 verification,
  NOT caused by it (reproduced on unmodified `main` @ `dc93931`).**
  `tests/concurrent_writers.rs::cross_row_update_deadlock_resolves_no_hang`
  under CPU contention (run the test binary 6× in parallel, filter
  `cross_row`) intermittently ends with **3 visible rows instead of 2** after
  two threads churn cross-row UPDATEs on a B-tree-indexed table — a
  superseded/aborted version stays visible to a later scan. ~1–5/6 parallel
  instances fail per round (Linux, 18 cores, debug); always green in
  isolation, so per-PR gates never caught it. **Blocks the toggle's planned
  default-ON flip.** Filed: `backlog_index.md` "Next up" item 16 + known-issue
  section in `docs/backlog/index_write_concurrency.md`.
- ~~**Read-only transactions pay a full commit fsync for nothing**~~
  **FIXED 2026-07-08** (branch `m9-group-commit`): `TransactionManager::
  commit` skips `commit_user_txn` when `undo_log.is_empty()`. Point SELECT
  ~3.05 ms → 1.09 µs. See `docs/backlog/group_commit_and_read_concurrency.md`.
- ~~**Deferred-sync (group-commit) mode has no buffer-pool
  force-WAL-on-evict yet**~~ **FIXED 2026-07-08** (branch `m9-group-commit`,
  design-doc item 6a): the buffer pool now tracks the durable WAL frontier
  (`durable_wal_lsn`) and `find_victim` writes back + evicts a dirty page
  once its LSN is durable (ARIES steal); `BufferPool::fetch_page_for_write`
  (used by every heap write/undo path + FSM scan) forces one `Wal::sync()`
  and retries when the pool is full of not-yet-durable dirty pages. Deferred
  mode is now unconditionally safe. Proven by `bufferpool.rs::
  fetch_for_write_forces_wal_sync_to_evict_nondurable_dirty_pages`; crash
  harness green.
- FSM is a linear scan over all heap pages — fine for M0/M1, revisit if insert
  throughput regresses.
- **`DbError::BufferPoolFull` at large single-table scale (discovered M6,
  not fixed):** a table growing into the hundreds of pages can exhaust the
  fixed 256-frame buffer pool (`POOL_CAPACITY` in `lib.rs`) even with
  small, individually-committed transactions — found while benchmarking
  `benches/btree.rs` at 100,000 rows across two tables. Per-transaction
  pinned-page accumulation was the first suspect but switching to one
  commit per 500-row batch didn't fully resolve it, pointing at the FSM
  linear-scan issue above compounding with page-count growth rather than a
  purely per-transaction pinning bug. Not investigated further — `benches/
  btree.rs` scopes its largest tier down to 10,000 rows instead. Revisit
  alongside the FSM item above if a real workload needs single tables
  larger than this. **Largely addressed 2026-07-08** (branch
  `m9-group-commit`, design-doc item 6a): the root cause was that
  `find_victim` could *never* evict a dirty page (its D5 hint was hardwired
  to `INVALID_LSN`), so a pool full of dirty pages had no victim. It now
  writes back + evicts dirty pages once their WAL is durable (and
  `fetch_page_for_write` force-syncs when needed), so the write path no
  longer dead-ends at `BufferPoolFull`. The FSM linear-scan cost above is
  separate and still open; a dedicated large-single-table stress test
  wasn't added, so this is "largely addressed," not formally closed.
- WAL truncation rewrites the entire file — acceptable for now, needs a proper
  log-segment scheme in later milestones.
- **No vacuum/GC in M1.** Dead tuple versions (`xmax` set, no snapshot can see
  them, or self-stamped-dead by an abort) are never reclaimed. Heap pages only
  grow. Safe (no correctness issue) but a real throughput/storage regression
  for update-heavy workloads — tracked for a post-M1 vacuum milestone. This
  compounds with the FSM linear-scan tech debt above (dead tuples reduce
  effective free space per page). Catalog pages have the exact same
  accumulate-garbage-on-rewrite property (M1.c) — every `CREATE TABLE`/RLS
  policy change leaves the previous catalog blob's page behind.
- **INSERT/UPDATE are ~2x slower than M0** when each statement is its own
  transaction (the worst case — see `PROGRESS.md`'s M1 entry for why this is
  expected and how batching multiple statements per transaction amortizes
  it away). Not a bug, but worth remembering when reading raw throughput
  numbers out of context.
- **No wait queue / deadlock detection in `LockManager`** (M1.b) — deliberate
  per D12, since SI's conflict handling is "abort immediately," not
  "block and wait." A future SERIALIZABLE/SSI effort would need to add this,
  which is exactly what the D11 seam exists to make possible without a
  lock-manager rewrite.
- **RC's EvalPlanQual re-evaluation path is unimplemented** (see Open
  questions above) — tracked, not silently dropped.
- **Catalog DDL is not transactional** (see Open questions above) — tracked,
  not silently dropped.
- SQL grammar gaps, all deliberate per the agreed M1 scope: no joins, no
  aggregates, no subqueries, no `ORDER BY`/`LIMIT`, `WHERE` is AND-only (no
  `OR`), no `@>` JSON containment, no binary JSONB storage, no JSON index.
- **`instant-distance` has no incremental insert** (see M2.b design note
  above) — `VectorIndex` rebuilds the whole HNSW graph from scratch on
  every `upsert`/`remove`, O(n log n) per insert rather than the O(log n)
  amortized a true incremental HNSW would give. Not a correctness issue;
  flagged for M2.d's benchmark table to quantify honestly at realistic row
  counts, since CLAUDE.md's §6 explicitly wants this evidence-based rather
  than assumed fine.
- **No vector-index cleanup on UPDATE** (see M2.b design note above) — a
  row's old vector value stays indexed forever under its now-dead `RowId`
  after an UPDATE (which always creates a new `RowId` in M1's MVCC design).
  Correctness is unaffected (stale candidates resolve to `NoVisibleVersion`
  and get filtered at read time), but it's an unbounded space leak under
  update-heavy workloads on indexed columns — the same shape of gap as M1's
  "no vacuum" tech debt, just for the secondary index instead of the heap.
  The same applies to `InvertedIndex` (M2.c) for the identical reason.
- **No full-text query SQL surface** — `InvertedIndex::search` exists and
  is tested directly, but there's no SQL-level way to call it; only `NEAR`
  (vector) has a `WHERE`-clause operator in M2's scope. Not a bug — flagged
  so it isn't mistaken for an oversight later.
- **`instant-distance`'s full-rebuild-per-upsert cost is measurable, not
  just theoretical** (see M2.d's benchmark table in `PROGRESS.md`):
  vector-index-active INSERT throughput was ~2.8x slower than without an
  index at just 200 rows in this milestone's benchmark. Not a correctness
  issue, and still off the foreground's *blocking* path (the mechanism is
  CPU contention between the foreground and worker threads, not a
  synchronous wait) — but real enough that "row write is the only
  synchronous cost" needs the asterisk "...but the worker's own cost isn't
  free, and it scales worse than a true incremental HNSW would." Flagged
  for a future milestone to revisit if it becomes a real blocker.
- **`EdgeIndex` has no abort-time (or update-time) cleanup** (M3.d design
  note above) — an aborted or logically-superseded edge's index entry is
  never retracted, an unbounded space leak under abort/update-heavy
  workloads on indexed `from_id`s. Correctness is unaffected (proven by
  `tests/graph_mvcc.rs`); the same shape of gap as M2's secondary-index
  cleanup gap and M1's "no vacuum" gap before that.
- **No Cypher `CREATE`/`DELETE` mutation surface** (M3.c) — the locked v1
  grammar is read-only (`MATCH ... WHERE ... RETURN`); `create_edge`/
  `delete_edge` are Rust-API-only, mirroring M1's `set_rls_policy`/M2's
  `set_column_index` precedent.
- **Graph nodes are opaque `i64` IDs only** (M3 confirmed scope decision)
  — no `:label` node-type declarations, no property-graph joins to a
  backing table. `a.name`/`b.name` are rejected with a clear parse-time
  error, not silently mis-parsed. A property-graph join model is a future
  extension once a real workload demands it.
- **Cypher v1 supports exactly one fixed-length directed hop** — no
  `OPTIONAL MATCH`, no variable-length paths (`*1..3`), no aggregation, no
  `ORDER BY`/`LIMIT`. Deliberate "practical subset" scope, matching the
  SQL layer's own precedent of excluding joins/aggregates/subqueries.
- **`poll_events` has no predicate pushdown** (M4.b) — cost scales with
  `__events__`'s total row count, not consumer lag or `limit`, quantified
  in `PROGRESS.md`'s M4 benchmark table (linear: 100→1,000→5,000 rows is
  ~10x→~4.8x time increases matching the size increases almost exactly).
  `vacuum_events` (M4.c) is the only current lever that bounds this cost —
  a `seq`-ordered secondary index is the natural future fix once this
  becomes a real bottleneck in practice, not before.
- **`__consumers__`'s `ack_events`-driven `heap.update` accumulates dead
  tuple versions with no cleanup** (M4.b) — the same "no vacuum" shape
  already accepted for the heap itself (M1), `VectorIndex`/`InvertedIndex`
  (M2), and `EdgeIndex` (M3), just for a new structure.
  `Engine::vacuum_events` (M4.c) reclaims `__events__` rows only; it does
  not touch `__consumers__`'s own dead versions — an asymmetry worth
  tracking explicitly since a future reader might otherwise assume
  `vacuum_events` cleans up both tables.
- **`apply_rls` is bypassed by `poll_events`/`ack_events`/`vacuum_events`
  entirely, by construction** (M4) — they are bespoke `Engine` methods,
  not `execute_sql`-routed plans, exactly like `edges_from` (M3).
  Consistent with existing precedent, not a new gap.
- **`vacuum_events`'s per-row cost is fsync-dominated, same root cause as
  every other multi-row mutation path** (M4.c/M4.d) — quantified in
  `PROGRESS.md`'s M4 benchmark table at a remarkably consistent ~3.06–3.10
  ms/row regardless of how many rows are reclaimed (100 vs. 5,000),
  because each reclaimed row's `heap.delete` is its own WAL-bracketed
  mini-txn (D2) that fsyncs independently. Not queue-specific; the same
  gap M1/M2/M3 already found and documented for every other per-row write
  path in this codebase — `vacuum_events` simply inherits it rather than
  introducing a new instance of it.

---

## Session log (append newest at top; use the real current date)

### 2026-07-16 — Item 49: report.sh "indefinite hang" investigation + Postgres connect-timeout fix

- **Trigger**: user observed "many metrics reports are not working and running
  in indefinite mode especially reports.sh" and asked for a root-cause
  investigation (script code, latest merged main changes, config adoption)
  before generating the benchmark used to plan the next optimization pass.
- **Investigated and ruled out** (expert-lens review per CLAUDE.md §0.6,
  applied to the freshly-merged item 47/44 PR #119 first since it's the most
  recent change to the write path): `DiskBTree::patch_many` and
  `Heap::delete_many` both hold exactly one page/leaf latch at a time, drop it
  before any fallback/recursive call, and process leaves in consistent
  ascending-key order across concurrent callers — no self- or cross-txn
  deadlock. `lock_mgr.try_acquire_write` is `WaitPolicy::NoWait` (aborts
  instead of blocking, per the SI design in CLAUDE.md D12). The item-15
  parallel-scan worker governor (`src/sql/parallel_scan.rs`) is non-blocking
  admission control (`take_from_pool` never waits; degrades to serial).
  `conc_matrix`'s `run_with_deadline` already bounds any real deadlock to a
  120s-per-cell "HANG" verdict on an isolated, fresh, tempdir-scoped engine —
  confirmed no cross-cell blast radius (`open_engine` opens a new `tempdir()`
  per cell).
- **Root cause found**: `benches/decompose.rs` opened every Postgres
  connection via `postgres::Client::connect(url, NoTls)` — 24 call sites, zero
  of them setting a `connect_timeout`. Empirically confirmed on this host: a
  refused TCP connect fails in 5ms; a connect to a black-holed/unresponsive
  address is still pending past 8s (`tcp_syn_retries=6` → ~2 min ceiling per
  attempt). A `PG_URL` that's merely unreachable (not actively refused —
  wrong host, firewalled, Postgres container mid-startup, a stale value left
  from a prior session) silently stalls the entire report with no output,
  exactly matching "indefinite mode."
- **Fix**: new `pg_dial(url) -> Result<Client, Box<dyn Error + Send + Sync>>`
  helper — parses `url` as `postgres::Config`, sets
  `.connect_timeout(Duration)` (default 10s, `PG_CONNECT_TIMEOUT_SECS`
  override) before `.connect(NoTls)`. All 24 raw `Client::connect` call sites
  (mechanical `sed` + 2 manual `match` sites) now route through it. Same
  `Result<Client, _>` shape, so `.unwrap()`/`.ok()`/`match` call sites needed
  no further changes.
- **Verified**: `UNIDB_BENCH=mmreport` run direct against a black-holed
  `PG_URL` completed in **14.6s total** (prints `[pg] WARNING: ... connect
  failed ... — skipping`, report finishes) — previously would have hung ~2min
  on the first connect alone. Re-ran against a real local Postgres 16
  (installed + started via the pre-existing `postgresql-16` apt package, root
  access) — full report completes normally, numbers unaffected (timeout never
  fires when the server responds).
- Gates: `cargo build --release --bench decompose` clean; `cargo clippy
  --release --bench decompose -- -D warnings` clean. Bench-harness-only —
  no engine/format/WAL change, no crash-harness re-run needed.
- Also fixed while touched: `PROGRESS.md`'s duplicate "Items 47 + 44" entry
  was found truncated mid-sentence (pre-existing, part of PR #119 as merged)
  — closed the sentence with a dated correction note (additive, not rewritten)
  rather than building the new entry on top of a broken doc. `backlog_index.md`
  rows for items 44/47 flipped from stale "NOT STARTED" to "SHIPPED (PR #119)"
  since main already carries that work.
- New full 1k/10k-row multi-model report generated on this branch with real
  Postgres comparison columns (local Postgres 16, matched `fsync`/`fsync`
  durability lens) — see `docs/performance/multi_model_report_20260716_*.md`
  for the numbers used to decide the next optimization target.
- Branch: `49-pg-connect-timeout` (based on latest `origin/main`, i.e. up to
  and including PR #119).

### 2026-07-16 — Items 47 + 44: UPDATE B-tree in-place patch + DELETE batched mini-txn (PR pending)

- **Item 47 — root cause found**: `stage_row_index_writes_update` was calling
  `patch_many` per-row with a single entry for the unique-enforcement index
  (PRIMARY KEY `id`). 250 rows × 1 FPI per row per leaf × 8KB = 2 MB extra WAL,
  driving WAL B/row from the expected ~100 to **8770** in the first naive
  implementation. Only the secondary BTree was being accumulated into
  `patch_batches`; the unique index was calling `patch_many` immediately.
- **Fix**: `init_patch_batches` extended to create a `PatchColBatch` entry for
  every `col.unique_index_root` (unique-enforcement index, added by item 35)
  in addition to `col.index_root` (secondary BTree). `stage_row_index_writes_update`
  unchanged-key path for unique indexes now pushes into `patch_batches` and
  `flush_patch_batches` calls `DiskBTree::patch_many` once per non-empty batch
  after the full row loop. `#[allow(clippy::too_many_arguments)]` added (8 args).
- **Item 47 measured result**: WAL B/row **619 → 465** (−25%) at 500-row scale.
  FPI savings grow with table size because more rows share the same leaf pages.
- **Item 44 measured result**: WAL B/row **230 → 107** (−53%) at 5000 rows,
  throughput **416k rec/s**. `Heap::delete_many` groups already-page-sorted
  RowIds by page_id; one WAL mini-txn per page instead of per row.
- **macOS scale constraint**: UPDATE throughput at 10k rows even with
  `deferred_sync=true` accumulates ~13ms/row from per-mini-txn mmap operations,
  WAL BufWriter writes, and mutex acquisitions — NOT a code regression. Item 47
  test uses 500 rows (completes in 9ms; threshold 570 < baseline 619 proves
  improvement). Item 44 test uses 10k rows (12ms for 5000 deletes thanks to batching).
- Crash harness: 38/38 (unchanged). Clippy: clean. fmt: clean.
- Backlog docs: `47_update_delete_write_throughput.md` → SHIPPED (Phase A);
  `44_bulk_delete_batched_wal.md` → SHIPPED.
- PROGRESS.md updated with "Items 47 + 44" entry (WAL B/row before/after,
  invariant analysis).
- Branch: `47-44-perf-batch`. PR pending.

### 2026-07-15 — Items 46 + 48: GROUP BY decode pushdown + DELETE all O(1) fast path (PR #117)

- **Item 46**: Extended B2 partial-column decode to the aggregate path in
  `src/sql/query_exec.rs`. `SELECT COUNT(*) GROUP BY g` now calls `deform_row`
  with a 1-column mask instead of `decode_row` (all columns). Confirmed:
  cols/row 4.00 → 1.00; unidb SELECT grouped: 4,947,561 → 6,611,524 rec/s (+34%).
- **Item 48**: `exec_delete` with `predicate = None`, no FK children, no CDC
  routes through `catalog.exclusive()?.truncate()` (O(pages)) instead of
  xmax-stamping N rows. WAL B/row: 196 → 1. unidb DELETE all: 303,892 →
  28,160,725 rec/s (92.7×). Now 7.35× faster than PG (was 0.23×, losing).
- **Bug found and fixed**: `stmt_uses_shared_catalog` returned `true` for
  no-predicate DELETE (shared DML lock), but the fast path calls
  `catalog.exclusive()?.truncate()` (needs exclusive lock) → panic at runtime.
  Fix: split Delete arm — `predicate: None` always takes exclusive path.
  Confirmed: 407/407 lib tests pass.
- **Item 45** (small-candidate guard): `MIN_PAGES=64` guard already existed as
  `PARALLEL_CANDIDATE_MIN` in `parallel_scan.rs`. Named/documented in the
  backlog; no code change needed.
- Bench Postgres caveat: new `pg-bench` Docker container runs without
  `wal_sync_method=fsync_writethrough`. PG write-op ratios (INSERT, UPDATE,
  DELETE selected) are not comparable to prior matched-durability runs. Unidb
  absolute numbers and read-op ratios are valid.
- Branch: `48-46-45-perf-batch`, PR #117. Backlog items 46 and 48 flipped to
  SHIPPED. PROGRESS.md updated with before/after table.
- **Next**: items 47 (UPDATE skip unchanged-key B-tree re-insert, WAL B/row
  618 → ~100) and 44 (per-page batched WAL for predicated DELETE) in a new
  worktree from main after PR #117 merges.

### 2026-07-15 — Item 43: A3 gate size-aware selectivity (SHIPPED, PR pending)

- Root-cause: `exec_select` had NO selectivity gate; it always called
  `try_exec_select_btree`, and `find_indexable_btree_predicate` picked `k >= 0`
  (sel=1.0) over `k < N` (sel=0.5) for `WHERE k >= 0 AND k < N`, fetching ALL
  rows via the B-tree at every scale.  No crossover was possible.
- Added `find_best_indexable_btree_predicate`: for AND predicates, uses ANALYZE
  stats to pick the most selective sargable arm.  For `k >= 0 AND k < N`, prefers
  `k < N` (sel=0.5) → B-tree returns only 50% of rows.
- Added size-aware cost model to `index_lookup_is_selective`:
  `prefer_index = page_count > BTREE_STARTUP_PAGES + matched_rows × HEAP_FETCH_SEQ_EQUIV`
  (`BTREE_STARTUP_PAGES=4.0`, `HEAP_FETCH_SEQ_EQUIV=0.012`). Crossover at ~2600
  rows for 50% selectivity.  Old catalogs (page_count=0) fall back to legacy
  0.3 threshold — no re-ANALYZE required for existing data.
- Added gate to `exec_select` (was only in `matching_rows`). Both SELECT and
  UPDATE/DELETE paths now respect the size-aware cost decision.
- Added `page_count` to `TableStats` (via `ANALYZE`, `heap.scan_pages()`).
- Empirical verification via `cols/matched` metric (debug build):
  BEFORE: 5.00 at all scales (always scan or non-selective B-tree).
  AFTER: 5.00 at ≤2000 rows (scan), 4.00 at ≥6000 rows (selective B-tree k<N). ✓
- New test file `tests/a3_gate.rs` (3 tests): size-swept correctness, no-ANALYZE
  fallback, 50%-selective DELETE regression guard.
- 50%-selective DELETE regression (CLAUDE.md §0.6.5) confirmed safe: 2000-row
  table, gate says scan, a3_gate test passes. ✓
- All gates green: 435 workspace tests, 38/38 crash harness, clippy, fmt.
- PR #115 opened (branch `43-a3-gate-size-aware`); DO NOT MERGE until an
  independent bench validation run (no Postgres connection needed for unit/crash
  gates, but the full MM_CRUD_ROWS=20000 report run is required for sign-off).
- Post-commit isolation probe (2026-07-15): parallel_resolve_candidates DOES
  fire for this query — 18 workers, 0 serial fallbacks. Isolation rec/s: 4.02M
  (bench: 1.78M; 2.25× difference is mmap page-cache state after 20k per-row
  INSERT fsyncs). Remaining gap vs PG (4M vs 6.4M, 1.6×) is per-row
  Vec<Literal>/String allocation + thread-spawn per query. PROGRESS.md
  corrected (removed incorrect claim that parallel_resolve_candidates didn't
  target B-tree candidates).

### 2026-07-15 — Items 39/42: PK/FK stress bench + bench harness buffer-pool fix

- Picked up item 39 (already committed by the user as `a6c56ba` on branch
  `39-pk-fk-relational-stress-bench` — Table 5 PK/FK relational-integrity
  stress in `benches/decompose.rs`) to verify with real numbers.
- Generating the full-scale report exposed a second, more consequential bug:
  `decompose.rs` never sizes its buffer pool (plain `Engine::open()` at all
  18 call sites), so Table 3.1's 1,000,000-row point hit `BufferPoolFull`
  and collapsed to 1,228 rec/s — the identical pathology diagnosed for the
  `unidb-studio` demo earlier the same day, now found in the project's own
  measurement tooling.
- Fixed: `bench_engine_open()` helper routes every bench engine through
  `Engine::open_with_pool_capacity` at 2,000,000 frames. Verified directly
  (smoke test at the exact scale that exposed it): 1,228 -> 15,905 rec/s at
  1M rows, ~13x, flat and consistent with the unaffected 10k-row point.
  Filed as its own backlog item (42) since it's more consequential than
  item 39 alone — past reports at large sweep sizes may have understated
  unidb's real performance.
- Encountered and cleaned up an orphaned duplicate report process (started
  5:15am, ~2.5h runtime, from before this session segment) competing with a
  fresh run for CPU — killed both, relaunched clean. Also killed an
  unrelated stray `unidb-server-full` on port 8080 belonging to a different
  session's checkout (`testing_unidb_engine_main`, not this one) at the
  user's explicit instruction.
- Full official-scale report (default MM_SIZES etc.) was still running after
  Table 4's 100k-txn point alone took ~13 minutes combined (documented as
  slow "by design" -- synchronous HNSW/graph index builds swept to
  millions); switched to a small-sweep rerun (`MM_SIZES=100,1000`,
  `MM_BULK_SIZES=1000,10000`, `MM_TX_SWEEP=100,1000`, `MM_CRUD_ROWS=1000`,
  `MM_FK_ORDERS=1000`, `MM_SAMPLE=50`) for real, complete, fast numbers --
  saved as `docs/performance/multi_model_report_20260715_091035.md` (62 MiB
  peak RSS, all 5 tables, both Table 5 correctness proofs pass on both
  engines).
- Also fixed a stale `backlog_index.md` header inconsistency found along the
  way (two conflicting "next new file" notes, 41 vs 42 — item 41 turned out
  to already be registered by a separate parallel session; true next number
  was 42, now 43).
- Gates: build/clippy/fmt clean, `cargo test --workspace` all green, crash
  harness 38/38 unchanged (bench-only change, no engine/WAL/format touched).
- `PROGRESS.md` entries added for both items. Branch
  `39-pk-fk-relational-stress-bench`, PR pending.

### 2026-07-14 — Item 41: NEAR() vec_distance virtual column

- **Problem:** `SELECT id, title, vec_distance FROM t WHERE NEAR(...)`
  returned `COLUMN_NOT_FOUND` — `exec_select_near` (`src/sql/executor.rs`)
  already computes the exact re-ranked Euclidean distance for every
  candidate to sort them, but discarded it after sorting instead of
  threading it through to projection.
- **Fix:** new `project_row_near` helper (alongside `project_row`) resolves
  every projected name normally except the reserved virtual name
  `vec_distance` (`VEC_DISTANCE_COL` const), which it substitutes with
  `Literal::Float(distance as f64)`. `SELECT *` (empty projection) still
  falls through to plain `project_row`, so the virtual column never leaks
  into `SELECT *` output. Outside a `NEAR` predicate nothing changed —
  `vec_distance` was never added to any catalog, so the existing column
  lookup already raises `COLUMN_NOT_FOUND` there.
- **Spec correction (inline, §9):** the spec's 4th acceptance criterion asked
  to update `vector_demo.py`; grepped the whole repo — no such file (or any
  Python demo) exists anywhere in this codebase. Marked as N/A with a dated
  note in `41_near_vec_distance.md` rather than silently dropping it;
  substituted an equivalent integration test that seeds the spec's own
  example corpus and asserts the same values/order.
- **Tests:** `tests/vec_distance.rs`, 3 new tests — ascending order + exact
  distance values for a known corpus (mirrors the spec's example table),
  `COLUMN_NOT_FOUND` outside `NEAR`, `SELECT *` never includes it.
- Gates: `cargo fmt --all --check` clean, `cargo clippy --workspace
  --all-targets -- -D warnings` clean, `cargo test --workspace --features
  server` green. No storage/WAL/format touch → crash harness unaffected, no
  new crash point needed. No `FORMAT_VERSION` bump, no §3 decision touched,
  no API/catalog change (matches the spec's declared scope).
- Docs: `41_near_vec_distance.md` → SHIPPED; `backlog_index.md` row 41 →
  SHIPPED; `PROGRESS.md` item 41 entry added; `engine_access_guide.md` §2
  SQL-surface list gained a `vec_distance` bullet.
- Branch `claude/near-vec-distance-docs-ysqyvn`.

### 2026-07-15 — Item 40: B-tree index sort-then-bulk-load backfill

- **Baseline measured first (CLAUDE.md §0.6.4):** 134.2 s for `CREATE INDEX
  USING BTREE (customer_id)` on 540k randomised-order rows, release build,
  `UNIDB_BUFFER_POOL_PAGES=1000000`. Root cause: N = 540k individual
  `DiskBTree::insert` calls → N mini-txns → N fsyncs.
- **Fix:** three-phase sort-then-bulk-load in `exec_create_index`
  (BTree+FullText paths; HNSW already collects into a Vec — untouched):
  Phase 1 collect (key, row_id) into `Vec`; Phase 2 `sort_unstable_by key`;
  Phase 3 `tree.insert_many(&pairs, pool, wal)` — one WAL mini-txn, one fsync.
  `insert_many` already existed for the coalesced-UPDATE path (A1/item 14).
- **After: 12.0 s — 11.2× speedup** (acceptance ≥ 5×, met).
- **Architecture verification (§0.6.2):** confirmed sorted input → rightmost-leaf
  inserts → pages fill to ~90-95%; one fsync vs 540k is the dominant win.
  MVCC: existing `snapshot_for_statement` before heap scan is unchanged and
  correct. Crash-safety: bulk mini-txn is all-or-nothing; catalog update is
  after. No new FORMAT_VERSION.
- **P40 crash test added** (`tests/crash/main.rs`): (a) heap rows committed
  before CREATE INDEX survive a no-checkpoint crash; (b) committed bulk-built
  index survives no-checkpoint crash and is queryable on reopen. 38/38 total.
- Gates: fmt/clippy clean, `cargo test --workspace` all green, crash 38/38.
- `PROGRESS.md` entry added. `docs/backlog/40_btree_bulk_build.md` → SHIPPED.
- Branch `40-btree-bulk-build`, PR #107 (MERGED).

### 2026-07-14 — Default buffer-pool capacity 4096 -> 65536 frames

- Debugged a "poor demo performance" report that surfaced *after* items 35/36
  were confirmed shipped and correct — two separate causes found in sequence
  (`unidb-studio` `DEMO_GUIDE.md` PRs #11, #12): a debug-vs-release build
  default, then this buffer-pool sizing gap.
- Root cause: `DEFAULT_POOL_CAPACITY = 4096` (32 MiB) is exhausted by a single
  demo table well before seeding finishes; `fetch_page_for_write` forces a
  synchronous `wal.sync()` on `BufferPoolFull`, independent of the normal
  size-based checkpoint trigger — measured 93 checkpoints for 211 commits at
  the old default, throughput collapsing ~15-20x.
- Self-corrected an initial recommendation: first assumed a
  Postgres-`shared_buffers`-style RAM tradeoff and suggested a conservative
  pool size. Verified against source (`struct Frame`, `BufferPool::open`) that
  this is wrong for unidb's mmap-backed architecture — the pool is pure
  pin/dirty-tracking metadata, not a page-data cache. Measured directly (RSS,
  micro-benchmark of `Engine::open()` cost at several capacities, a full
  `unidb-studio --size 5M` run: 4,077,283 rows, 0 evictions, 586 MiB RSS).
- Chose 65536 (512 MiB ceiling, ~35µs/open) as a modest default bump —
  matches the P1.c 256->4096 precedent — rather than a much larger number,
  because the frame table is allocated eagerly at open and a huge default
  would tax every `Engine::open()` including ~50 test files and tiny embedded
  use. Filed a follow-up backlog item for lazy/growable frame allocation to
  remove that tradeoff properly.
- Gates: build clean, sync invariant empty, fmt/clippy clean, crash harness
  37/37, `cargo test --workspace` all green (excl. the pre-existing unrelated
  `slow_query_captured_after_threshold_set` timing flake).
- `PROGRESS.md` entry added. Branch `bump-default-buffer-pool-capacity`, PR
  pending.

### 2026-07-14 — Item 35 follow-up: concurrent-INSERT PK race fix, PR #102 MERGED

- Found that two concurrent INSERTs racing the same PK/UNIQUE value could both
  pass `enforce_unique` (neither saw the other's uncommitted row under MVCC) and
  both commit — visible duplicate.
- Fix: `RecordKind::UniqueKey` phantom lock in `lockmgr.rs`. `exec_insert`
  acquires exclusive lock (keyed by `hash(table, col, value)`) via
  `WaitPolicy::Wait` BEFORE `snapshot_for_statement`. Loser blocks until winner
  commits, then takes fresh snapshot → sees committed row → `UniqueViolation`.
  Lock released via `release_all` at commit/abort.
- New `pk-unique-race` conc_matrix cell: 6 writers × 20 rounds, CONC_REPEATS=10;
  10/10 PASS (toggle off + on). Closes acceptance checkbox from spec correction
  in PR #101.
- `PROGRESS.md` updated with follow-up fix section.
- **PR #102 MERGED** (commits `a0958e3` + `e91f120` + `fca5eda`).
- **Next:** item 36 — FK row-level enforcement (reuses `unique_index_root`).

### 2026-07-14 — Unique-index enforcement (item 35), branch `35-unique-index-enforcement`

**Phase 0 (baseline):**
- Measured PK-vs-no-PK degradation: PK 5,484→1,936→1,167 rec/s at 5k/10k/15k
  (O(n²) degrading); no-PK 115,279→113,783 rec/s flat.
- Found blind spot: `benches/decompose.rs` `sql_bulk_insert` used no-PK table
  (`id INT`); Table 3.1 never exercised `enforce_unique`.

**Phase 1 (fix):**
- Added `unique_index_root: Option<PageId>` to `ColumnDef` with
  `#[serde(default)]` (no FORMAT_VERSION bump, old catalogs open with `None`).
- Added `set_column_unique_index_root()` to `Catalog`.
- `exec_create_table`: after `create_table()`, auto-creates implicit `DiskBTree`
  per indexable PK/UNIQUE column (INT64/TEXT/BOOL); stores meta page in catalog.
- `apply_durable_index_writes` (INSERT path): maintains implicit unique index.
- `stage_row_index_writes` (UPDATE path): maintains implicit unique index for new version.
- `enforce_unique`: fast path (single-column, indexable) = `DiskBTree::search_eq`
  + `get_visible` MVCC re-check per candidate; fallback heap scan for composite/
  non-indexable sets.
- Fixed all ColumnDef literal sites (13+ occurrences across catalog.rs,
  executor.rs, graph/edges.rs, large_object.rs, queue/mod.rs, parser.rs,
  plan.rs, optimizer.rs, queue/payload.rs, sql/logical.rs).

**Phase 2 (correctness):**
- P35 crash test: create PK table → insert committed row → crash (no checkpoint)
  → reopen → duplicate still rejected, new row accepted. 37/37 crash tests pass.
- 6 regression tests: flat-throughput checks (PK INSERT, UNIQUE INSERT,
  PK UPDATE); MVCC inv. 1 (dead index entry from UPDATE not misread as live);
  MVCC inv. 2 (same-batch PK duplicate caught); NULL distinctness.

**Phase 3 (results):**
- PK INSERT after: 27,046→28,276→30,362 rec/s at 5k/10k/15k (flat ~23-26×).
- Table 3.1 PK'd (10k/1M/2M): 19,695/16,817/16,489 rec/s (flat O(log n)).
- W4/W0 ladder: 1.30×/1.29×/1.29× (unchanged, ladder table has no PK).
- Changed `sql_bulk_insert` to use `id INT PRIMARY KEY` — closes blind spot.
- Regenerated `docs/performance/multi_model_report_20260714_190433.md`.

**Docs updated:**
- `35_unique_constraint_full_scan.md` → SHIPPED 2026-07-14
- `backlog_index.md` row 35 → ✅, row 36 → TOP PRIORITY
- `engine_access_guide.md` — `is_unique` note updated (implicit internal B-tree)
- `README.md` — item 35 milestone row; D7 crash count (37 tests)
- `PROGRESS.md` — item 35 entry with all numbers

**Gates:** fmt ✅, clippy ✅, workspace tests ✅ (435+), crash 37/37 ✅.
Branch `35-unique-index-enforcement` ready for PR.

**Next up:** item 36 (FK row-level enforcement — now unblocked, reuses item 35's
`unique_index_root` for the parent PK lookup).

### 2026-07-14 — Observability API gaps (item 34), branch `34-observability-api-gaps`

**Part A — Slow-query threshold configuration:**
- `UNIDB_SLOW_QUERY_MS` env var wired in `src/bin/unidb-server.rs`: read at
  startup, calls `EngineHandle::set_slow_query_threshold(ms).await` before
  `AppState::new()`. 0 or absent = disabled (existing default preserved).
- `PUT /config/slow_query_threshold_ms` route added: superuser-gated (same
  `ensure_superuser` gate as `PUT /tables/{table}/rls` and `POST /admin/flush`),
  calls same `set_slow_query_threshold` setter, returns 204. Already atomic
  (`AtomicU64`) — no lock contention.
- `SlowQueryThresholdRequest` added to `src/server/dto.rs`.
- `EngineHandle::set_slow_query_threshold(threshold_ms: u64)` async method added.
- No new capture machinery — `Engine::note_query_time` / `slow_queries` ring
  already existed (P6.g); this change merely wires the setter that was always there.

**Part B — Stats-history ring buffer:**
- `StatsPoint` (raw capture struct, `pub(crate)`) + `StatsHistoryPoint` (`pub`,
  serde-serializable, with rate fields) added to `src/lib.rs`.
- `STATS_HISTORY_MAX = 300` constant. `stats_history: Mutex<VecDeque<StatsPoint>>`
  + `stats_ticker_handle: Mutex<Option<StatsTickerHandle>>` fields added to `Engine`.
- `Engine::capture_stats_point(&self)` (`pub`): captures current counters into
  ring, pops oldest if len > 300.
- `Engine::stats_history_snapshot(n: usize) -> Vec<StatsHistoryPoint>`: takes last
  n entries, computes `commits_per_sec`/`wal_bytes_per_sec` from consecutive dt_ms
  delta; first point rates = 0.0; oldest-first; empty Vec on fresh engine.
- `src/stats_ticker.rs` — new file, exact autovacuum pattern: `Shutdown` condvar,
  `StatsTickerHandle` (Weak<Engine>, bounded-join Drop, self-join guard),
  `worker_loop` (5 s interruptible sleep), `Engine::spawn_stats_ticker`.
- `EngineHandle::spawn` calls `engine.spawn_stats_ticker()` after
  `spawn_autovacuum()` — ticker never starts for bare `Engine::open()`.
- `GET /stats/history?points=60&interval_ms=5000` handler + route added.
- `HistoryQuery` added to `src/server/dto.rs`.
- `EngineHandle::stats_history(n: usize)` async method added.

**Tests:** `tests/server_observability.rs` — 9 tests:
- HTTP: PUT returns 204; slow query captured after threshold=1ms; threshold=0
  disables capture; GET /stats/history empty on fresh engine; interval_ms echoed;
  points param capped at 300.
- Engine unit: rate-fields-correct (two-capture sequence with commit; validates
  first-point rates = 0, second-point commits_per_sec > 0, oldest-first order);
  ring caps at 300; snapshot(n < len) returns most-recent n.

**Gates:** crash harness 35/35 UNCHANGED (pure in-memory ring, no WAL/format
touch); `cargo test --workspace --features server` all green (0 failures);
`clippy --workspace --all-targets -- -D warnings` clean; `cargo fmt --all` clean.
No §3 decision reopened; no tokio on engine core; ticker uses std::thread only.

**Docs:** `docs/REST_API.md` — two new route sections; `docs/engine_access_guide.md`
§13 added; `docs/backlog/34_observability_api_gaps.md` → IN PROGRESS;
`backlog_index.md` row 34 → IN PROGRESS. PR open — STOP for review.

### 2026-07-14 — Studio API readiness (item 30), branch `30-studio-api-readiness`

- **E1 (G9 LIKE/ILIKE):** `Expr::Like` + `QExpr::Like` on both expression paths.
  `like_match()` / `like_match_chars()` Unicode-correct matcher. `eval_expr`,
  `eval_qexpr`, `Runner::eval` all handle `Like`. All traversal functions updated
  (`bind_expr`, `collect_columns`, `validate_expr`, etc.). NULL propagation
  correct. `plan_is_concurrent_read` unchanged (LIKE runs on both paths).
- **E2 (G11 MATCH):** `Expr::Match` + `QExpr::Match`. `find_match()`. 
  `exec_select_match()` via FULLTEXT DiskBTree (over-fetch-then-filter, mirrors
  `exec_select_near`). `plan_is_concurrent_read` updated to exclude MATCH.
  `eval_expr` returns `Bool(true)` for re-check. QExpr path uses inline
  `fulltext::tokenize` check (no index on planner path).
- **Bug fixed:** `unidb-logical/src/apply.rs` `make_event` test helper missing
  `before`/`after`/`ts_ms` fields from item-29 Event struct.
- **Tests:** 23 new `tests/like_match.rs` (differential LIKE/NOT LIKE/ILIKE +
  MATCH coverage; LIKE uses `PRAGMA case_sensitive_like = ON` in SQLite; ILIKE
  uses `lower(col) LIKE lower(pattern)` SQLite equivalent).
- **E3 (integration guide):** §12 "ERP app walkthrough" added to
  `engine_access_guide.md`; §2 "Supported" updated for LIKE+MATCH+ILIKE. 
  `documentation_index.md` updated. Spec `30_studio_api_readiness.md` →
  SHIPPED; `backlog_index.md` row 30 → ✅; `19_sql_surface_gaps.md` G9+G11
  already annotated "(Delivered under item 30)"; PROGRESS.md entry added.
- **Gates:** `cargo test -p unidb` ✓, `--features server` ✓, `--workspace` ✓,
  crash 35/35 ✓, `clippy --workspace --all-targets -D warnings` ✓, `fmt` ✓.
- **Next:** push, open PR — STOP for review, do not merge.

### 2026-07-13 — Engine-architecture PDF reference added to `docs/design/` (docs-only), branch `claude/engine-architecture-pdf-doc-ft0k64`, PR #56

- Added **`docs/design/unidb_engine_architecture.pdf`** (+ its
  `unidb_engine_architecture.html` source for regeneration): a shareable,
  self-contained architecture reference distilled from
  `engine_design.md`/`CLAUDE.md`/`PROGRESS.md`/`positioning.md`/`roadmap.md`.
  Contents: all components with 8 diagrams (layer stack, deployment/HA
  topology, page/tuple layout, write path + group commit, ARIES recovery,
  MVCC version chain, IVF-Flat `NEAR` flow, moat-vs-replaced-stack), how each
  subsystem works, the measured performance-improvements ledger (group commit
  ~7.7×, COUNT 2.81× vs PG, parallel scan 6.4–6.6×, crabbing +25%, replaced
  stack 3.61×, W4/W0 1.12–1.20×), locked decisions D1–D13, the honest
  limitations registry, and a **future-scope section aligning against Postgres
  (tiers P0–P3: transactional DDL/savepoints/full SSI → SQL surface → perf
  parity → wire protocol/ops) and Supabase (auth platform, SDKs, PostgREST-style
  API, push realtime, dashboard; edge functions kept a non-goal)** for
  production readiness.
- Docs-only: **no engine code touched, no §3 decision reopened, no tests/bench
  affected.** Registered in `docs/design/design_index.md` +
  `docs/documentation_index.md` (with the headless-Chromium regeneration
  command). The PDF is a distilled snapshot — `CLAUDE.md`/`PROGRESS.md` win on
  disagreement.
- **Rebased onto post-merge `main` (PRs #57–#59) and refreshed for staleness**
  before landing: one index-file conflict resolved (kept Milestone 18's
  `engine_access_guide.md` entry alongside the PDF entry); the PDF/HTML updated
  so it isn't stale on arrival — `JOIN … USING` moved from out-of-scope to
  shipped (§5.3/§13/§14, PR #58), Milestone 18's `information_schema.*` /
  `unidb_catalog.*` introspection contract noted in §5.1 (PR #57), and the
  §14.2 Supabase tracks annotated as now-filed backlog items 20–24 (PR #59).
- **Second pass — folded in the four PRs that merged later the same day**
  (#60–#63, rebased onto `72b98f1`): **Milestone 20** realtime dispatcher (new
  §8.1 — ephemeral SSE resume + `unidb-dispatch` fan-out with proven
  at-least-once/zero-loss-across-crash + webhook→dead-letter), **item 21**
  observability metrics and **item 22** logs surface (new §9.1.1 + §9 table +
  §10 ops bullet — lock-free chokepoint metrics via `stats()`/`/metrics`, JSON
  logs + `request_id` correlation + bounded `GET /logs`). §14.2 Supabase table
  flipped: items 20/21/22 now **SHIPPED** (with a shipped tag), 23/24 remain
  filed. Cover "Covers:" line + footer updated. Still docs-only; **no engine
  code, no §3 decision, no format/crash-surface touched.**

### 2026-07-13 — `UNIDB_CONCURRENT_SQL_WRITES` default-ON flip (item 11 follow-up), branch `11-concurrent-writes-default-on`

- Completed item 11's filed follow-up: flipped `UNIDB_CONCURRENT_SQL_WRITES`
  **default-ON**. Measurement + docs only; no correctness work (item 16 was the
  soak blocker and is fixed/merged, PR #50; matrix already 28/28 on main).
- **Baseline first (unflipped build), Table C:** indexed 8-writer 811 (toggle
  off) vs 1013 (toggle on) — confirmed the win exists before touching the default.
- **Flip mechanism:** `env_flag` → `env_flag_default_on` (unset ⇒ true; only
  `0`/`false`/`off`/`no` force off). Field/setter/env doc comments un-"ships
  dark"; conc_matrix bench legend now names *on* as production default; toggle-off
  test doc updated. Runtime setter + serialized `cat_write` path unchanged.
- **Flipped Table C (no env):** indexed 8-writer **1016** (matches toggle-on
  baseline 1013 ⇒ default is ON); `=0` override → 741 (serialized regime ⇒ revert
  works). **+25% vs +38% prior art — reported honestly** (same mechanism, absolute
  varies by machine; not chasing the lucky run).
- **Gates:** `-p unidb` + `--features server` + `--workspace` pass; crash **31/31**;
  clippy `--all-targets --features server -D warnings` clean; fmt clean.
  Concurrency matrix **28/28 @ `CONC_REPEATS=10`** (committed dated report). Peak
  RSS ~31.4 MB (bench process, unchanged by flip).
- **Docs closeout (§9):** README, engine_design §5.2/§5.4 + doc-version footer,
  processing-engines 06/10 notes, high_scale_concurrency, backlog `index_write_
  concurrency` (flip note + DoD ✅) + `backlog_index` item 1 + item-16 DoD line +
  PROGRESS (new entry + item-11 promise ✅) + this MEMORY entry.
- **Next:** push, open PR (backlog item 11 follow-up + item 16 spec, measurement
  table, one-env-var revert story), STOP for review. Do not merge.

### 2026-07-13 — Post-item-16 full QA battery on merged `main` — PASS (production-ready gates)

- Ran the complete validation battery as three sequential tiers on `main`
  @ `fb33c4d` (item-16 fix merged), per CLAUDE.md §7/§8:
  - **Tier 1 (functional/regression):** default suite, crash harness 31/31,
    server suite, workspace (attach/embed), `concurrent_writers` standalone
    ×7 (M7 lesson), loom model, clippy `-D warnings`, fmt — all green.
  - **Tier 2 (concurrency stress):** full 28-cell matrix at
    `CONC_REPEATS=10` + 18 spinners — **28 PASS · 0 FAIL** (280 clean
    executions, toggle off AND on). Report committed:
    `docs/performance/conc_matrix_20260713_041032.md`. The item-16 fix
    holds at its acceptance gate; matrix legend updated (anomaly fixed —
    cells are now its permanent regression gate).
  - **Tier 3 (load/scale):** native multi-model report, baseline-matched
    knobs vs the committed 2026-07-10 report — ladder W0/W4 within noise
    (no regression from the abort-ordering fix); bulk scan **improved
    2.7×** at ≥1M rows (17.5M vs 6.2M rec/s — item-15 parallel scan
    default-ON now visible). PG column skipped (`PG_URL` unset) — absolute
    unidb numbers are the regression signal. Report committed:
    `docs/performance/multi_model_report_20260713_041622.md`.
- **One defect found & fixed (test, not engine):** `server_txn::concurrent_
  request_on_busy_session_is_409_txn_busy` failed 8/8 standalone — a timing
  knife-edge (3000-statement batch vs fixed 200 ms probe), proven
  pre-existing on pre-fix `main` (187986c), NOT an item-16 regression.
  Rewritten with probe-until-busy loop + TXN_BUSY-aware retry; 12/12 green;
  merged PR #51 (`f3df160`). Diagnosis recorded an engine fact: the SQL
  session path takes row locks **NoWait** (waiter gets WRITE_CONFLICT, not
  a park) — documented in the test for future authors.
- Item-16 lane worktree `../unidb-item16` removed post-merge.

### 2026-07-12 — Item 16 root-caused + fixed (abort ordering); matrix 17/11 → 28/0

- Worked backlog item 16 end-to-end on branch `16-visibility-fix` (worktree).
  Read the spec, MEMORY, and the MVCC/txn/heap/bufferpool/lockmgr/executor
  paths under the §0.6 lens before touching anything.
- **Root cause (one bug for all 3 symptom classes):** `TransactionManager::
  abort` removed the aborting xid from `active` *before* physically undoing its
  heap writes. Because visibility has no "aborted" state (not-active-and-in-range
  ⇒ committed), a concurrent snapshot in that window saw the aborting txn's
  doomed new UPDATE version as committed (and the old one it superseded as
  invisible). The new version's RowId is unlocked (`heap.update` locks only the
  old version), so a concurrent writer could chain onto it → undo then restores
  the old version → **two live versions of one id (persistent dup) or none
  (missing row)**. D5-flush error + >120 s hang were downstream of this, not
  separate bugs.
- **Instrument-first, per plan.** Added a `#[cfg(test)]` abort-midpoint seam +
  a deterministic unit test (`aborting_txn_new_version_never_visible_to_
  concurrent_snapshot`) that pins an observer scan to the abort midpoint —
  proved pre-fix it reads doomed `"v2"`, not a plausible story. Also temporarily
  re-introduced the bug to confirm the SQL-level regression test
  (`item16_readers_during_cross_row_churn_{off,on}`, 8w×8rows+2r) fails pre-fix
  without external load (lost/gained row, COUNT disagree, >90 s hang), then
  restored.
- **Fix (single-site, `txn.rs::abort`):** undo + WAL-abort while the xid is
  still `active`; remove from `active` / mark aborted / `release_all` only after.
  Toggle-off byte-behavior unchanged otherwise; no format change; crash harness
  untouched (recovery undo is single-threaded — window never exposed there).
- **Validation:** conc matrix **28 PASS/0 FAIL** at `CONC_REPEATS=10`, 18
  spinners, toggle off AND on (was 17/11). D5 + hang did not recur. Gates: lib
  374 + all integration green, crash harness 31, clippy `-D warnings` + fmt
  clean. Peak RSS ~9.7 MB. **No §3 decision reopened (D5 not touched).**
- Docs: spec file dated root-cause + Status→SHIPPED; `backlog_index.md` row 16 +
  "Next up" (item 11 flip now unblocked); `PROGRESS.md` entry (before/after
  matrix + peak RSS); `engine_design.md` §4.1/§4.3 + footer inline corrections.

### 2026-07-12 — Concurrency correctness matrix built; item 16 found to be toggle-independent + worse

- User asked (reacting to the item-16 intermittent failure) for `scripts/
  report.sh` to be enriched with border-case concurrent read/write testing,
  all meaningful permutations, tabular pass/fail output, under the §0.6 lens.
- Built `benches/conc_matrix.rs` (28 correctness cells: 9 workload families ×
  toggle × index × reader isolation; oracles = exact visible-id set, no dup
  ids in any snapshot, COUNT(*) agreement, RR/SER repeatable re-reads, sum
  invariance, index-vs-scan agreement; spinner-based CPU contention; repeats;
  hang deadline → FAIL row, matrix continues). Wired into `report.sh` (matrix
  appended to every report; `--conc` fast path; CONC_* knobs; `.gitignore` +
  `scripts_guide.md` updated). clippy `-D warnings` + fmt clean.
- **Findings (release, macOS native, `main` @ `0c09a70`): the item-16 MVCC
  anomaly family reproduces with the toggle OFF — the production default is
  affected**, contradicting the 2026-07-11 "production default unaffected"
  note (corrected in `backlog_index.md` + `index_write_concurrency.md`, as an
  inline correction per §9): transfer-sum short RC snapshot 7/10; vacuum×churn
  persistent post-quiescence duplicate ids 3/10; 8w cross-row churn dup ids
  1/6. Toggle ON: up to 10/10, plus a **D5-violation commit error** and a
  **>120 s hang** under contention — three distinct symptom classes for the
  item-16 root-cause to explain (visibility, WAL/flush ordering, deadlock).
  Official full-matrix run: **17 PASS · 11 FAIL of 28**; with spinners even
  the original 2w×2rows geometry fails 2/3 (without them it passes 6/6 —
  why the shipped test looked reliable).
- Committed on branch `conc-correctness-matrix`, rebased onto `main` after
  PR #45 (item 17) merged. Note: PR #45's *body* still says "backlog item
  16" — stale pre-renumber labels; that PR is item **17** (replaced-stack
  headline), unrelated to this anomaly.

### 2026-07-11 — Cross-domain headline vs replaced stack (item 17), branch `mm-replaced-stack-headline`

Redirected from HOT/A2 after a critical-lens review: HOT reopened locked decision
D4 for ~0.42× on a single-model bench §1 says we should lose — **deferred it**.
Instead sharpened the §6 differentiator (backlog item 17; the crash tests are named
`item16_*` — written before a rebase renumbered the backlog entry 16 → 17 to avoid
colliding with main's already-merged item-16 MVCC-anomaly follow-up).

- **Found the headline was dishonest:** Table 4 ("one atomic txn vs the replaced
  stack") actually compared unidb's 4-model commit against a *single PG relational
  row*. Replaced with a real replaced-stack baseline (row + pgvector+HNSW + graph
  adjacency + outbox, four independent commits, no shared txn).
- **Measurement-hygiene catch (the session's key lesson):** the first fair-Docker
  run showed **~parity** (0.9–1.6×, noisy) — I did NOT headline it. Root cause:
  Docker VM `fsync` is cheap/buffered for both, masking unidb's "1 sync vs 4"
  edge. The *correct* lens is matched **AND expensive** durable sync. Native run
  (unidb `F_FULLFSYNC` vs local pgvector Postgres `fsync_writethrough`) → stable
  **3.61×** (250 vs 69 txns/s). Both lenses reported honestly in PROGRESS/README.
  My original ~3–4× prediction was right — it just needed the expensive-sync lens.
- **Crash-consistency proof (unconditional win):** 2 new `tests/crash` tests
  (`item16_incomplete_four_model_txn_leaves_zero_orphans`,
  `item16_committed_four_model_txn_survives_intact`) — harness **29 → 31**. Stack
  side (`pg_stack_torn_record_demo`) shows the torn record.
- Fixed a real bug found by running it: `$2::vector` made PG infer the param as
  `vector` (WrongType panic) → `$2::text::vector`. Infra: `pgvector/pgvector:pg18`
  image + `MM_REPLACED_STACK=1` toggle. Benches + docs only; no §3; clippy/fmt clean.

### 2026-07-11 — REST API enrichment (item 12) shipped, branch `claude/rest-api-enrichment-vly934`

- Implemented all four checkpoints of `docs/backlog/rest_api_enrichment.md`
  (the last NOT-STARTED item): R1 transaction sessions (`X-Txn-Id`,
  begin/commit/rollback, busy→409, principal→403, idle reaper, stale→404),
  R2 one-shot `isolation` on `/sql`, R3 `POST /events/vacuum` +
  superuser-gated `PUT /tables/{t}/rls` (`Engine::set_rls_policy_sql`,
  SQL-predicate-string policy) + `POST /admin/flush`, R4 `POST /rows/batch`
  + principal-bound idle-expiring result cursors. Server-layer only; crash
  harness stays 29; sync invariant clean.
- +24 integration tests (`server_txn.rs`, `server_enrich.rs`, registered
  with `required-features`); `ApiError` → enum; unit tests for the session
  registry + cursor store. Full battery green: 373 default + 29 crash +
  server suites, clippy/fmt/workspace clean.
- Self-initiated benchmark (§0.6): sessions amortize commit fsyncs — 100
  INSERTs 161.3→33.9 ms (**4.8×**); batch insert 500 rows 718.4→35.0 ms
  (**20.5×**); peak RSS 43 MB. Recorded in `PROGRESS.md`.
- §9 staleness fixed in passing: `REST_API.md` intro (retired writer-thread
  description) + incomplete error table; `engine_design.md` §8/§9/RLS/
  module-map/footer; README status/env-table/layout/attach-client notes.
- **Found (and proved pre-existing on `main`): MVCC visibility anomaly under
  `UNIDB_CONCURRENT_SQL_WRITES` when the box is CPU-contended** — 3 visible
  rows instead of 2 in `cross_row_update_deadlock_resolves_no_hang`; filed
  as item 16 + known-issue in `index_write_concurrency.md`; blocks that
  toggle's default-ON flip. Production default (off) unaffected.
- PR #43 raised and merged same day (squash, `9635f7f`); the PR-reference
  docs fix landed as an immediate follow-up PR.
- **Next** candidates: item 16 (root-cause the anomaly),
  17 HOT update, parallel-scan follow-ups, attach-client sessions.

### 2026-07-11 — Expert lens codified in CLAUDE.md §0.6, branch `claude/report-script-performance-efcszq`

Docs-only; no engine code touched. User request, in two rounds:

- **Round 1 — added CLAUDE.md §0 step 6 + §0.6 "Expert lens — senior database
  architect & designer (every session, every action)."** Distills the six
  practices that produced the `report.sh`-arc wins, each anchored to a real
  incident: re-derive ROI order (Phase B's B2-leads reorder), verify THIS
  engine's storage model before importing another engine's hazards/optimizations
  (the nonexistent pool-vs-mmap landmine; the provably-incorrect index-skip),
  find the real code path + config (`try_exec_select_btree`; the default-off
  parallel toggle behind "no parallel win"), prove empirically with clean
  measurement, gate by measured conditions (A3 selectivity), and escalate
  honestly with sign-off.
- **Round 2 — the user corrected my first draft's history, and the correction IS
  the lesson:** `report.sh` was not built proactively — the **user had to ask
  for the stress testing** (and supply the details), and separately had to ask
  for the architect-level review. Rewrote §0.6's preamble to state that honest
  history, and added **item 0: initiate stress testing/benchmarking yourself,
  unprompted** — scale sweeps, concurrency, churn, crash points, baseline
  comparison per §6 — for every shipped change and periodically for the whole
  system. "The user asked for a stress test" is now defined as a process
  failure on my part. §0 step 6 updated to say the same.
- Section numbered **§0.6** (a subsection of §0) so existing §0.5/§3/§6/§9
  cross-references elsewhere in the docs stay valid — no renumbering.

### 2026-07-11 — Parallel worker governance + default-on, branch `parallel-worker-governance`

Backlog item 15 (`15_parallel_worker_governance.md`). Commit `df068bb`.

- **Root-caused a user report** ("report.sh shows no parallel improvement"):
  verified in code that parallel scan was `ENABLED = false` (default-off) AND
  nothing in `decompose.rs`/`scripts/`/`docker/` set `UNIDB_PARALLEL_SCAN` — so
  `report.sh` ran the *serial* path. Reproduced both: serial 5.6M vs parallel 35M
  at 1M, same code path. **Neither the report nor my earlier metrics were wrong —
  different configs** (the toggle). Lesson: a shipped-but-dark feature is invisible
  to the canonical benchmark; wire the bench (or default-on) so it reflects reality.
- **The user pushed on "why off?"** — the honest answer was real governance gaps
  (verified: no global worker cap → M×N oversubscription; no timeout propagation
  into workers), not caution. Built the governance: G1 global cap (WorkerLease
  admission), G2 deadline/cancel snapshot into workers, G3 load-tests, G4 flip
  default-on. Now `report.sh` shows the win by default.
- First backlog item under the new numbering convention: created
  `15_parallel_worker_governance.md`, registered #15 in `docs/backlog/backlog_index.md`.
  Read-only; crash 29; default-on but `UNIDB_PARALLEL_SCAN=0` reverts.

### 2026-07-11 — Parallel filtered SELECT, branch `parallel-index-select`

Milestone P follow-up. Commit `78f63a1`. After PR #37 merged, picked the *honest*
highest-value remainder — **not** SUM/GROUP BY (I'd over-stated its ROI; GROUP BY
is already ~0.8–0.9× vs PG), but the filtered `SELECT` which was still the worst
÷PG in the suite (~0.14×). It routes through `try_exec_select_btree`'s serial
candidate loop; parallelized it via `parallel_resolve_candidates` (partition the
candidate RowIds) + `heap::get_visible` (extracted per-RowId resolve). **6.41×**
at 500k rows. Same primitive shape as the filtered COUNT that got 6.6× — the B2
per-row closure reused directly. Read-only; crash 29; default-off. Lesson
reinforced: re-check ROI honestly before grinding the thing you named earlier
(see [[critical-architect-review]]).

### 2026-07-10 — Milestone P: parallel scan workers, branch `parallel-scan`

Built parallel scan (P-primitive → P-a → P-b). Commit `9a82d97`. Detail in
`PROGRESS.md`'s "Milestone P" entry + the Current-status bullet.

- **De-risked the gating question first** (per my own Phase-B architect review):
  read `read_handle.rs` + `bufferpool.rs` and found the pool-vs-mmap staleness
  landmine **does not exist** — unidb is mmap-as-storage (owned-copy reads under
  the mmap read-lock), so parallel workers always see committed data. My earlier
  flag was a Postgres-shaped hazard that doesn't apply here. Lesson: verify the
  storage model before importing another engine's hazards.
- **The wiring gotcha:** the bench's scan workloads route through `query_exec`
  (filtered COUNT → Scan→Filter→Aggregate) and `try_exec_select_btree`, NOT the
  bare `exec_select` full scan — so I had to parallelize `query_exec::scan`, not
  just `exec_select`, to touch the actual scan gap.
- **Honest ROI:** unfiltered `COUNT(*)` **3.82×** (parallel_count does the whole
  count in workers) — strong, beats PG. Filtered scan only **1.59×** because only
  the base Scan is parallel and Filter+Aggregate are a serial Amdahl tail; partial
  aggregate (predicate+count into workers) is the filed next lever, not forced in.
- `std::thread::scope` (not tokio, §4); `rayon` seen in `cargo tree` is
  `instant-distance`'s, pre-existing + sync. Crash 29 (read-only). Default-off
  toggle pending a soak. See [[unidb-benchmark-measurement-hygiene]].

### 2026-07-10 — CRUD performance Phase B (read path), branch `crud-perf-phaseB`

Executed Phase B (C1′ → B2 → B1 → B5). Commits: `73f8a93` C1′ (cols/row),
`c03eab0` B2, `f47859c` B1, `88115bc` B5. Full detail in `PROGRESS.md`'s "CRUD
performance — Phase B" entry + the Current-status bullet above.

- **User asked me to review the plan as a 20+yr DB architect** (see
  [[critical-architect-review]]) — reordered by real ROI (B2 leads, not B1),
  fixed B2 to the PG `heap_deform_tuple` `natts` stop, split parallel scan into
  its own milestone (`docs/backlog/parallel_scan.md`) with the pool/mmap
  read-consistency landmine surfaced, and added B5 (bitmap-style access) for the
  OLTP pattern the microbench hides.
- **B1 over-delivered: `SELECT COUNT(*)` beats Postgres 2.81×** (81.4M vs 29.0M
  rec/s) by counting visible slots via headers, decoding nothing.
- **B2 works but the filtered-SELECT ≥0.5× target isn't met** — dec/row 2→0,
  cols/row 8→5, +28% absolute, but the query projects `body` (still materialized
  for matches) and PG's tight scan leads; the scan gap needs parallel scan.
- **Implementation gotcha:** Table 3's `SELECT … WHERE k>=0` routes through
  `try_exec_select_btree` (the index picks up the sargable `k>=0`), NOT the
  full-scan `exec_select` loop — so B2 had to be wired into the btree candidate
  path too, not just `exec_select`.
- Measurement: PG-side variance is large (SELECT filtered PG swung 1.9M→6.9M
  rec/s), so trust unidb absolute + dec/row/cols/row over a single-run ÷PG (see
  [[unidb-benchmark-measurement-hygiene]]). Crash harness 29 unchanged
  (read-only). No `FORMAT_VERSION` bump; no §3 decision touched.

### 2026-07-10 — CRUD performance Phase A (write path), branch `crud-perf-phaseA`

Executed Phase A of `docs/backlog/crud_performance.md` (C1 → A1 → A3 →
A4; A2 deferred). Commits: `7ba6aad` C1 instrumentation, `da1194c` A1 coalesce,
`c63a509` A3+A4, `c8c9c1c` bench ANALYZE. Full detail in `PROGRESS.md`'s "CRUD
performance — Phase A" entry and the Current-status bullet above.

- **Discovered the plan's A1 ("skip unchanged-column index maintenance") is
  incorrect on this engine** and proved it empirically (a point `SELECT WHERE
  k=x` returned `[]` after a non-key UPDATE with the index write skipped —
  because `heap.update` writes a new RowId, the chain is backward-only, and
  `heap.get` never walks forward, so the B-tree is the only forward resolver).
  Paused, showed the user the evidence, and shipped the correct alternative
  (WAL coalescing via `DiskBTree::insert_many`) — same RC2 win (WAL 8868 → 619
  B/row, 14×), no correctness bug.
- **Discovered the ≥0.8× write-path acceptance is architecturally unreachable
  in scope** (residual UPDATE gap = insert-new-version MVCC cost → needs HOT/A2;
  DELETE gap = PG parallel/tight-C scan+mark-delete → needs Phase-B
  decode-pushdown). Paused, the user chose to ship the measured win + revise the
  acceptance + file A2 and Phase B as the path to parity.
- **A3 gated by selectivity** after measuring that an ungated index path
  *regressed* a 50%-selective DELETE; bench now `ANALYZE`s both engines so the
  gate routes correctly (UPDATE 25% → index, DELETE 50% → scan).
- **Measurement discipline note:** two early "regression" runs were contaminated
  by *stray `criterion` bench processes left alive from earlier background runs*
  (load avg ~5, 2–3 concurrent `decompose` procs). Lesson: `pkill -f decompose`
  and confirm a single process before trusting a bench delta — criterion does
  not exit when the parent shell is killed.
- Crash harness 28 → **29** (P29). No `FORMAT_VERSION` bump; no §3 decision
  reopened. Peak RSS ~18.5 MB (buffer-pool-bounded, unchanged).

### 2026-07-10 — Coordinator: post-merge verify + main-unbreak hotfix

- **Verified #28 (`GET /tables`) and #29 (durable on-disk FSM) after their merges
  to `main`.** The coordinator gate runs *both* the default `cargo test -p unidb`
  and `--features server` (a single worktree lane runs one); that caught a
  regression #28's own green PR had hidden — `tests/server_tables.rs` was never
  registered in `Cargo.toml` with `required-features = ["server"]`, so the
  default (no-server) test build auto-discovered it and failed to compile
  (`unresolved import server_common`, `cannot find crate tokio`). Fixed by adding
  the `[[test]]` block, mirroring the 13 existing server-test entries. `main` now
  green: crash harness 28/28, clippy/fmt clean, 0 async-deps.
- **durable-FSM verdict (measured, honest):** the `HeapFull` scaling ceiling the
  PR #25 Postgres baseline found is FIXED (dies ~876 pages before → clean to
  ≥2,000 after; insert cost flat ~17–28 µs/row vs. rising 65→173 then error). The
  requested concurrent-SQL-write refinement showed **no measurable improvement**
  (B3 microbench ~40 pages so `set_pages` rarely fired; concurrency was already
  fine via group-commit fsync) — recorded, not buried.
- **#27 (studio-UI spec) closed** as not-needed; worktrees `../unidb-fsm` +
  `../unidb-tables` removed (merged), `../unidb-pgbench` kept.
- Committed direct to `main` per user (build-unbreak + this handoff refresh).

### 2026-07-08 — M11 SQL constraints (SQL lane, branch `sql-constraints`)

- **New milestone proposed and implemented: M11 — SQL Constraints**
  (PK/FK/UNIQUE/NOT NULL/CHECK/DEFAULT), both column-level and table-level.
  Developed in the SQL-lane worktree (`../unidb-constraints`), disjoint from
  the Core lane (M10 vacuum) and Surface lane — no storage-core files
  (`heap`/`bufferpool`/`wal`/`txn`/`mvcc`/`recovery`/`read_handle`) touched and
  `lib.rs` untouched, per the roadmap's parallel-lane rules. Full entry with
  design rationale in `PROGRESS.md`'s M11 section.
- **Root gap closed:** `sql/parser.rs::convert_create_table` previously read
  only a column's name + data type and **dropped `c.options` entirely** — all
  constraint clauses were silently ignored. It now maps every column option
  and table constraint into new catalog fields.
- **Catalog model:** `ColumnConstraints` (grouped into one `#[serde(default)]`
  field on `ColumnDef`) + `TableConstraints` (one field on `TableDef`), plus
  `ForeignKeyRef`/`ForeignKey`. All `#[serde(default)]` → pre-M11 catalog blobs
  deserialize unchanged (no `FORMAT_VERSION` bump). Dropped `ColumnDef`'s `Eq`
  derive (now carries `Expr`/`Literal`, not `Eq`); nothing depended on it.
- **Enforcement** (in `exec_insert`/`exec_update`, all reusing existing
  machinery): DEFAULT fill (INSERT only) → NOT NULL → CHECK (via `eval_expr`)
  → UNIQUE (synchronous heap scan) → FK referenced-table existence.
- **Deliberate deviation from the prompt, for correctness:** UNIQUE is a
  **synchronous heap scan**, NOT the M6 async B-Tree index. `IndexStatus::Ready`
  ≠ "reflects every write" (the M7 CSR-traversal bug); a stale index entry is a
  false "no conflict" that would admit duplicates. The heap scan is guaranteed
  current for the writer and sees its own uncommitted rows (so a dup *within one
  multi-row INSERT* is caught). B-Tree index stays a read accelerator only.
- **Scope calls:** FK = referenced-table existence only (no row-level RI /
  cascades; no `DROP TABLE` exists yet). CHECK inherits two-valued NULL
  semantics. Constraints apply to writes after `CREATE TABLE`, not retro-
  validated (no `ALTER TABLE ADD CONSTRAINT`).
- **Tests:** new `tests/constraints.rs` (12 tests). `cargo test -p unidb`
  (226 unit + 12 constraints + 11 crash + rest) and `--features server` both
  green; clippy `-D warnings` + fmt clean.
- Not merged to `main` this session; on branch `sql-constraints` pending
  hand-merge/PR. `server/error.rs` gained additive 4xx arms for the new error
  variants (needed for the all-features clippy gate) — flag at merge as a small
  cross-lane touch.

### 2026-07-08 — Track D: semantic search (cosine metric + `unidb-embed` CLI, branch `surface-embed`)

- **Surface lane, worktree `../unidb-embed`.** Disjoint from Core/SQL: the only
  engine file touched is `src/vector.rs`; everything else is a new
  workspace-member crate. Full write-up in `PROGRESS.md`'s Track D entry.
- **`src/vector.rs` — cosine metric (kept small):** new `pub enum Metric {
  Euclidean (#[default]), Cosine }`; `VectorIndex::with_metric`/`metric()`/
  `set_metric()`. Metric is per-index, carried on every `VectorPoint`, applied
  in both HNSW build and search. Cosine = `1 - cos` (`pgvector` `<=>`), zero-norm
  guarded. `set_metric` triggers a full `rebuild()` (graph edges were chosen by
  the old metric) — the "changing metric implies a rebuild" requirement.
  `VectorIndex::new()` still defaults Euclidean, so the `index_worker.rs:162`
  construction site is untouched (I did **not** edit index_worker/executor/
  catalog). 9 new unit tests; engine lib 225 → 234.
- **`unidb-embed/` crate:** CLI (`embed-insert`, `search`) that embeds text via a
  pluggable OpenAI-compatible HTTP endpoint (key via `UNIDB_EMBED_API_KEY`) and
  stores/searches through the REST server via `unidb-attach`. `embed.rs` (HTTP +
  response parse), `sql.rs` (pure tested SQL builders), `main.rs` (clap). 11
  tests. `README.md` has a worked example. Added to root `[workspace] members`.
- **Constraint honored:** embedding *generation* is client-side only — no model/
  network dep added to the `unidb` engine crate (`unidb-embed` pulls `reqwest` +
  `unidb-attach`, engine `[dependencies]` unchanged).
- **Gates:** `cargo test --workspace` green (234 engine lib + 11 embed + all
  server/attach/crash/concurrency); clippy `-D warnings` + fmt clean.
- **Follow-up (SQL lane, not this lane):** expose the metric through `CREATE
  INDEX ... USING HNSW <metric>` (catalog + executor); the engine API supports
  cosine today but nothing wires a per-`CREATE INDEX` metric choice yet.

### 2026-07-08 — 6b concurrent SQL SELECT (branch `m9-concurrent-select`)

- Extended 6b from point reads to **read-only SQL `SELECT`** on the
  concurrent read path (stacked on `m9-concurrent-reads`; PR #2 group-commit
  work already merged to `main`).
- **What landed:**
  - `Engine.catalog` → `Arc<RwLock<Catalog>>` (readers need the live
    `TableDef.pages`, which grows on INSERT). Writer takes the write-lock per
    statement only — never across an fsync (group-commit defers the fsync to
    a later step), so readers block only briefly. 16 catalog call sites in
    `lib.rs` routed through `cat_read`/`cat_write` free helpers (field-level
    borrow so other engine fields stay disjointly borrowable).
  - `executor::exec_select_readonly` — `PageReader`-generic full-scan SELECT
    reusing `decode_row`/`predicate_matches`/`project_row`;
    `plan_is_concurrent_read` (plain SELECT, no NEAR). `project_row`/
    `find_near` promoted to `pub(crate)`.
  - `ReadHandle::execute_sql` (read-only) + `is_concurrent_read_sql`
    classifier; `EngineHandle::execute_sql_read` (spawn_blocking); server
    `post_sql` routes concurrent-readable SQL to the read handle, everything
    else (writes/DDL/NEAR) to the writer thread.
- **Lock order** is consistent catalog → txn → mmap on both writer and reader
  sides (reader never holds catalog+txn simultaneously; both hold
  catalog-outer/mmap-inner) — no inversion/deadlock.
- **Verification:** new `concurrent_sql_select_...` test (4 readers `SELECT`
  while writer inserts 500; every row's `name` pairs with its `id` — catches
  torn reads / inconsistent catalog+snapshot). 232 unit + 25 server + 11
  crash + 2 concurrent_reads + unidb-attach green; clippy/fmt clean.
- **Still writer-thread by design:** `NEAR` (needs HNSW fast path),
  `edges_from`/Cypher, `poll_events` — additive same pattern if needed.

### 2026-07-08 — 6b concurrent read path: point reads (branch `m9-concurrent-reads`)

- Continued the concurrency track (item 6b of
  `docs/backlog/group_commit_and_read_concurrency.md`): take reads off the
  single writer thread. **Structure chosen with the user: a shared read
  handle**, not full interior-mutability of the engine (rejected because it
  would put a `Mutex` on the buffer-pool frames, and `find_victim`-must-flush
  while holding it is a reentrancy/deadlock trap). Writer keeps owning
  `Engine` with `&mut self` writes **unchanged**; only read-relevant state is
  shared.
- **Landed (stacked on `m9-group-commit`):**
  - `bufferpool.rs`: `mmap` → `Arc<RwLock<PageFileMmap>>` (guards against a
    reader seeing a torn/remapped-away page); `PageReader` trait (read seam)
    + `SharedPageReader` (frame-free reader). Writer methods stay `&mut self`,
    locking the mmap internally. Committed separately as "Phase 1a".
  - `heap.rs`: `get`/`scan` generic over `PageReader` (reads copy pages out,
    no pin/unpin).
  - `txn.rs`: `TransactionManager` state behind `Arc<Mutex<TxnInner>>`
    (`SharedTxn`); methods `&self`; `read_snapshot()` gives a self-contained
    RC snapshot for a read that allocates **no xid and writes no WAL**.
  - `read_handle.rs` (new): `ReadHandle` (`Send + Sync + Clone`) with
    `get(row_id)`; `Engine::read_handle()`. Server `GET /rows/:id` now
    dispatches to it via `spawn_blocking`, bypassing the writer channel.
- **Verification:** `tests/concurrent_reads.rs` (4 readers hammering
  committed rows while the writer inserts 1000 — exact bytes, no tears);
  `benches/server.rs::concurrent_read_throughput` shows reads scale (~3.0k →
  ~4.3k → ~4.5k reads/s at 1/10/50 clients; HTTP-client-bound microbench) vs
  the old flat writer-serialized path. 230 unit + 25 server + 11 crash + 1
  concurrent_reads + unidb-attach all green; clippy/fmt clean. `Engine` stays
  deliberately non-`Sync`; `ReadHandle` is the `Send + Sync` shared reader.
- **Remaining 6b slice:** concurrent SQL `SELECT`/`NEAR`/`edges_from`/`poll`
  — same pattern, needs `Engine.catalog` → `Arc<RwLock<Catalog>>` (readers
  need live `TableDef.pages`), a read-only executor path, and
  `ReadHandle::execute_sql`. Foundation (`PageReader`/`SharedTxn`) makes it
  additive. Documented in the design doc.

### 2026-07-08 — Group commit + read-only fsync skip (prototype, branch `m9-group-commit`)

- User goal: improve unidb's parallel/durable performance. Diagnosis (from
  the prior FFSDB-eval session) confirmed against source: the ~3–4 ms floor
  on every durable op is per-statement fsync, and the server serializes
  everything through one writer thread (flat throughput under concurrency).
- **Key source finding before touching anything:** an autocommit statement
  does **two** fsyncs, not one — the mini-txn commit (`wal.rs::
  commit_mini_txn`, D2, fires on *every* mutation) *and* the user-txn
  commit (`commit_user_txn`, M1). So group-committing only the user-txn
  level would have left the bigger per-statement floor untouched; the real
  win required deferring the mini-txn fsync too. Verified `recovery.rs`
  handles a read-only txn that writes no `WAL_TXN_COMMIT` (orphan BEGIN →
  incomplete-user-txn undo pass finds no mutations to reverse → harmless).
- **Implemented (default path + crash harness unchanged):**
  1. `txn.rs` — read-only skip: `commit` skips `commit_user_txn` when
     `undo_log.is_empty()`. Resolves the M1.d open question.
  2. `wal.rs` — `deferred_sync` flag gating fsync in all four commit/abort
     paths + public `sync()`. Off by default.
  3. `lib.rs` — `Engine::set_deferred_sync` / `sync_wal`.
  4. `server/engine_handle.rs` — `worker_loop` now drains all queued
     requests into a batch, runs in deferred mode, and issues **one fsync
     per batch** (`flush_pending`); commit/abort replies withheld until that
     fsync so a client never sees a non-durable commit. Reads/inserts reply
     immediately. Checkpoint forces a flush first.
- **Numbers (M5 Pro, measured this session):** concurrent `POST /sql`
  INSERT throughput ~131 / ~149 / ~153 → **~242 / ~756 / ~4,780 ops/s** at
  1/10/50 clients (flat → scaling; 31× at 50). Embedded point SELECT
  ~3.05 ms → **1.09 µs**.
- **Verification:** 228 unit + 25 server + 11 crash-harness tests green;
  clippy `-D warnings` + fmt clean. No §3 locked decision re-opened (D1/D2/
  D5 upheld — deferring the commit fsync only delays when `durable_lsn`
  advances; no page flushes ahead of the durable WAL).
- **Then item 6a landed (same session):** buffer-pool force-WAL-on-evict.
  `bufferpool.rs` now tracks `durable_wal_lsn` and `find_victim` writes back
  + evicts a dirty page once its WAL is durable (ARIES steal, was previously
  impossible — the D5 hint was hardwired to `INVALID_LSN`);
  `BufferPool::fetch_page_for_write(page_id, &mut Wal)` (used by every heap
  write/undo path + FSM scan) force-syncs the WAL and retries when the pool
  is full of not-yet-durable dirty pages. `Engine::sync_wal` now also
  refreshes the pool's frontier. New unit test
  `fetch_for_write_forces_wal_sync_to_evict_nondurable_dirty_pages` (229
  unit total); crash harness green (write-back-on-evict preserves recovery).
  This makes deferred mode unconditionally safe *and* largely fixes the M6
  `BufferPoolFull`-at-scale limitation.
- **Still not done (tracked in the design doc):** 6b concurrent read path
  (readers off the single writer thread — the one real architectural
  change, an addition to existing MVCC). "M9" filename is taken by the
  parked Python-bindings doc, so this track is documented descriptively.
- Not merged to `main` this session; on branch pending PR.

### 2026-07-08 — FFSDB eval comparison doc (no code change)

- User asked to eval <https://ffsdb.com/evals> against unidb and put the
  comparison under `docs/performance/fssdb` (and Postgres "whatever is
  possible"). FFS is the same competing project that prompted M6–M8.
- Fetched FFS's published evals (FFS `2.0.0-alpha.1`, Apple M-series):
  raw embedded index-primitive microbenchmarks (`ffs::BTree` vs sled,
  `ffs::Hnsw` vs instant-distance, `ffs::Csr` vs petgraph, plus
  Postgres+pgvector / LanceDB / Kùzu / Neo4j). **Central framing of the
  writeup: FFS benchmarks raw non-durable index structures (ns–µs); unidb
  benchmarks the full durable MVCC/WAL/SQL engine path (µs–ms). Direct
  ns-vs-ms ratios measure durability, not index quality, and are
  deliberately not headlined.**
- Re-ran unidb's own benches fresh on this box (**Apple M5 Pro**, Rust
  1.95, release): `graph` (adjacency 1k batched 75µs / 10k 744µs; edge
  insert 3.41ms), `btree` (indexed point/range ~3.1ms flat vs full-scan
  growing to ~4.5–4.9ms at 10k), `vector` (NEAR k=5 3.93ms; indexed insert
  11.75ms/row; fulltext primitive 13.86µs). Ran a fresh **Postgres 18.4 +
  pgvector 0.8.4** HNSW bench matching FFS's setup (10k×dim128, m=16/ef200):
  build 770ms, HNSW query 43.5µs server-side, brute 1556µs.
- **The one clean apples-to-apples: unidb's vector index *is*
  `instant-distance`** (it wraps the crate), and instant-distance is one of
  FFS's baselines — so FFS's "2.64× faster on query" is transitively a
  direct statement about unidb's vector core. That's the most meaningful
  single comparison and is called out as a real FFS win.
- Deliverables (docs-only, no code touched): `docs/performance/fssdb/`
  `README.md` (comparison + §5 Postgres head-to-heads, reusing M2/M3/M4
  recorded PG numbers incl. the ~100× queue-poll win), `raw-results.md`
  (provenance), `pgvector_bench.sql` (reproducer). Added a `performance/`
  pointer to `docs/documentation_index.md`. No milestone opened/closed; `PROGRESS.md`
  untouched (no feature shipped).

### 2026-07-07 — M8 (attach client) merged from worktree; M7 CSR-traversal bug found and fixed; M0-M8 all shipped

- User had M6/M7 landing on `main` while a separate `m8-attach-client` git
  worktree (`/Users/sagarmahamuni/Development/AI_World/unidb-m8-attach`)
  independently completed M8. Asked to verify it was safe to merge and
  commit. Confirmed it built, tested, clippy/fmt-clean, and preserved the
  "engine stays sync" invariant on its own branch before touching `main`.
- Merged by hand (not a literal `git merge`) since `MEMORY.md`/
  `PROGRESS.md` had diverged significantly on both branches: copied
  `unidb-attach/` and `docs/backlog/m8_attach_client_plan.md` wholesale,
  edited the root `Cargo.toml` to add the `[workspace]` table (their
  design — a mixed manifest, no file-moving needed — is better than my own
  earlier, reverted plan to move `src/`/`tests/`/`benches/` into a nested
  `unidb/` directory), and added the one missing `IndexKind::BTree` variant
  to `unidb-attach`'s local copy of that enum (the M8 branch predates M6).
- **Merge verification surfaced a real M7 bug**, not an M8 problem: running
  `cargo test -p unidb` in isolation (specifically to confirm the
  sync-invariant check wasn't accidentally relying on workspace-wide
  feature unification) intermittently failed. Isolating further
  (`cargo test -p unidb --test graph_mvcc
  aborted_edge_creation_never_surfaces_in_traversal`, repeated 30 times)
  reproduced the failure 100% of the time. Root cause and fix are in the
  corrected M7 design note above and `PROGRESS.md`'s M7 entry (which
  carries a correction block rather than being silently rewritten): M7's
  `graph_candidates` preferred the CSR graph index once `Ready`, but
  `Ready` doesn't mean "every write since is incorporated into the
  debounced rebuild" — a transaction's own just-created edge could be
  invisible to its own immediate `edges_from` call. Fixed by reverting
  `edges_from`/`execute_cypher` to call `EdgeIndex` directly and
  unconditionally (`src/graph/index.rs`, `src/lib.rs`,
  `src/graph/executor.rs`); removed the now-misleading CSR-preferring
  tests (`tests/graph_mvcc.rs`, `tests/graph_rebuild.rs`) since
  `edges_from`/Cypher no longer exercise that path at all. `CsrIndex`
  itself is unaffected — still built, live-upserted, rebuilt-on-open, and
  benchmarked; it's just not consulted for correctness-critical traversal
  right now.
- Full re-verification after the fix: `cargo build --workspace` clean;
  `cargo test --workspace` 228 unidb tests + 19 `unidb-attach` tests + 1
  doctest, all green; `cargo test -p unidb --test graph_mvcc` run 15x in a
  row, all green (race confirmed gone); `cargo test -p unidb` (225) and
  `cargo test -p unidb --features server` (228) both green; `cargo clippy
  --workspace --all-targets -- -D warnings` clean; `cargo fmt --all --
  check` clean (one formatting fix needed in `src/graph/executor.rs` after
  the signature revert).
- Updated `README.md` (status line, project layout — workspace + `unidb-
  attach/` tree, milestone table through M8, new "Rust attach client"
  section) and `docs/REST_API.md` (pointer section to the attach client).
  Confirmed `cargo bench -p unidb-attach --bench attach -- --test` runs
  successfully (attach-client overhead vs. direct `Engine` calls vs. raw
  `reqwest`, tracking M5's already-established HTTP-overhead finding, no
  new surprise).
- Result: **M0-M8 all shipped.** Standing next item, if this project
  continues, is the parked Phase 2 SQL capability plan (`docs/backlog/
  phase2_sql_capability_expansion.md`).

### 2026-07-07 — M7 (CSR graph index) complete; M7 milestone DONE

- **M7.a**: `IndexKind::Csr` (engine-managed only, no SQL keyword — exists
  purely to reuse `index_worker.rs`'s `(table, column)`-keyed machinery
  for `__edges__`'s `from_id`); new `src/csr_index.rs` (`CsrIndex`,
  classic sorted-offset-array CSR layout, `stage`/`rebuild` split so
  raw accumulation and the queryable structure are separate); restructured
  `index_worker.rs::worker_loop` from a plain `for msg in rx` into
  `apply_msg` + an explicit `try_recv()` drain loop, coalescing a burst of
  queued edge messages into one `rebuild_dirty` pass — the user-approved
  fix for HNSW's still-unfixed "rebuild on every single upsert" pattern.
  Debounce proven via a test-only `CsrIndex::rebuild_count()` counter:
  200 back-to-back messages produce far fewer than 200 rebuilds (see
  design note above for why "far fewer" is the honest, provable claim,
  not "exactly 1").
- **M7.b**: `graph::index::graph_candidates` — prefers CSR once `Ready`,
  falls back to the always-current `EdgeIndex` otherwise, with the
  correctness reasoning worked through explicitly (CSR's lag can only
  cause a missed very-recent edge, never a phantom one, since every
  candidate is re-validated against MVCC visibility regardless of source).
  Wired into `Engine::edges_from` and the Cypher executor's fast path
  (`execute` gained an `index_worker: &IndexHandle` parameter);
  `create_edge` sends a live CSR upsert alongside its existing synchronous
  `EdgeIndex.insert`; new `rebuild_csr_index` backfill function runs
  during `Engine::open`. `tests/graph_mvcc.rs` gained an explicit
  CSR-path MVCC proof (waits for `Ready`, mirrors M3's "single most
  important test" for the CSR-preferring path specifically).
- **M7.c**: extended `benches/graph.rs`'s `adjacency_scan` group with a
  `csr` variant — found CSR at parity with the already-fast `batched`
  (`EdgeIndex`) path (97.4µs vs 97.7µs at 1k edges, 998µs vs 972µs at
  10k), an honest non-win explained by the benchmark's single-hop shape
  not exercising CSR's actual advantage (multi-hop traversal, which
  Cypher doesn't support yet). Extended `tests/graph_rebuild.rs` with
  CSR restart-rebuild and delete-reflection tests (both explicitly wait
  for `Ready` to provably exercise the CSR path).
- Full verification: 225 unit tests (228 with `--features server`), all
  integration suites green (`graph_rebuild` 3->5, `graph_mvcc` 2->3),
  `cargo clippy --all-targets -- -D warnings` and `cargo fmt --all --check`
  clean both with and without `--features server`, `cargo tree
  --no-default-features --edges normal` still empty of server-only deps.
- `PROGRESS.md`'s M7 entry and this file's Current status/design
  notes/known-issues sections updated. Next: M8.a (Cargo workspace
  restructure + `unidb-attach` crate skeleton), per the approved plan.

### 2026-07-07 — M6 (B-Tree secondary index) complete; M6 milestone DONE

- Prompted by a comparison against a competing project (FFS/ffsdb) that
  publishes B-Tree/HNSW/CSR-graph benchmarks and embedded/standalone/
  attach deployment modes — user approved a 3-milestone follow-on plan
  (M6 B-Tree, M7 CSR graph, M8 attach client), researched via three
  parallel Explore agents plus direct synthesis (two Plan-agent dispatches
  hit a transient "529 Overloaded" error with zero output; the plan was
  written directly from the completed Explore-agent research instead of a
  third retry).
- **M6.a**: `IndexKind::BTree` (additive, `src/catalog.rs`); new
  `src/btree_index.rs` (`BTreeIndex`, `OrderedValue`, `RangeOp`) backed by
  `std::collections::BTreeMap` — zero new dependencies; `by_id: HashMap<
  RowId, OrderedValue>` bookkeeping so `upsert` can remove a stale bucket
  entry when a row's indexed value changes (new relative to `VectorIndex`/
  `InvertedIndex`, since a `BTreeMap` is keyed by value, not id);
  `index_worker.rs` wiring (`IndexedColumn::Ordered`, `SecondaryIndex::
  BTree`) into the existing generic worker machinery; `exec_create_index`
  validation extended (`Int64`/`Text`/`Bool` valid, `Vector`/`Json`
  rejected); parser `USING BTREE` support — discovered `sqlparser`'s
  `IndexType::BTree` is a *native* variant (not `Custom`, unlike `HNSW`/
  `FULLTEXT`), which broke a pre-existing "BTREE is unsupported" test
  immediately upon implementing (see design note above).
- **M6.b**: index-assisted `exec_select` — `find_indexable_btree_predicate`
  + `try_exec_select_btree`, reusing `exec_select_near`'s exact
  resolve-then-refilter template. Unlike M2's `HNSW`/`FullText` additions,
  this needed genuine new query-planning logic, not just wiring (see
  design note above) — including a stricter `IndexStatus::Ready` gate than
  `NEAR` needs, since an equality/range query can't tolerate an
  incomplete-but-silent result the way `NEAR`'s approximate top-k can.
  Differential test proves indexed and full-scan paths return identical
  rows; `tests/btree_mvcc.rs` proves an aborted insert never leaks through
  the index-assisted path; `btree_assisted_select_still_respects_rls`
  proves RLS still applies to every index-sourced candidate.
- **M6.c**: `benches/btree.rs` (point/range SELECT, indexed vs. full-scan,
  1,000/10,000 rows) — headline: indexed stays flat (~3.1 ms) while
  full-scan grows with table size (3.60->4.95 ms point, 3.66->4.54 ms
  range). Discovered and worked around a real, unrelated `BufferPoolFull`
  scaling limit at 100,000-row scale while building the benchmark (see
  design note above and the new "Known issues" entry) — not fixed, flagged
  for later. Extended `tests/index_rebuild.rs` with BTree restart-rebuild
  and pre-`Ready` fallback-correctness tests.
- Full verification: 222 unit tests (225 with `--features server`), all
  integration suites green, `cargo clippy --all-targets -- -D warnings`
  and `cargo fmt --all --check` clean both with and without `--features
  server`, `cargo tree --no-default-features --edges normal` still empty
  of server-only deps.
- `PROGRESS.md`'s M6 entry and this file's Current status/design
  notes/known-issues sections updated. Next: M7.a (`CsrIndex` +
  debounced/coalesced rebuild), per the approved plan.

### 2026-07-07 — M5.d complete; M5 milestone DONE; M0-M5 all shipped

- **M5.d**: full server integration test suite —
  `tests/server_common/mod.rs` (shared scaffolding: `TestServer`, JWT
  token helpers, `metrics_pair()`'s `OnceLock` memoization),
  `tests/server_crud.rs`, `server_sql.rs` (multi-statement abort-rolls-
  back-row-data, `Literal::Json`-as-real-nested-JSON), `server_cypher.rs`,
  `server_graph.rs`, `server_auth.rs` (5-case matrix), `server_events.rs`
  (SSE delivery + ack-stops-redelivery), `server_shutdown.rs` (graceful
  shutdown drains an in-flight request, preserves committed data),
  `server_metrics.rs` — 25 new tests total, each gated via its own
  `[[test]] required-features = ["server"]` Cargo.toml entry.
- **Required a real mid-checkpoint architecture fix**: `PrometheusMetricLayer::
  pair()` installs a process-global `metrics` recorder — calling it more
  than once in one process panics. Multiple test functions in one test
  binary each spawning an independent `TestServer` hit this immediately.
  Fixed by restructuring `build_router` to accept an already-obtained
  `(PrometheusMetricLayer, PrometheusHandle)` pair as an explicit argument
  rather than calling `pair()` internally — `unidb-server`'s own `main()`
  now calls `pair()` once at startup and passes it in, and the test
  helper/benchmark each memoize their own single pair via `OnceLock`.
  Production behavior is unchanged; this was purely a test-process
  concern that the original M5.c design hadn't needed to consider.
- **`benches/server.rs`** (new): direct `Engine::insert` (~6.30ms) vs.
  `POST /rows` (~6.69ms) — only ~6% HTTP/writer-thread overhead; JWT
  verification alone (~817ns, negligible); SSE polling at 1/10/50
  subscribers (~5.2ms/~33.9ms/~162.6ms — worse than linear, the concrete
  number behind `sse.rs`'s qualitative "N subscribers x poll interval x
  poll_events cost" warning); concurrent `POST /sql` throughput at
  1/10/50 clients (~135/~157/~158 ops/s — **flat**, not scaling with
  concurrency at all, landing in the same range M1's `benches/load.rs`
  already found for single-table INSERT). The flat-throughput number is
  the clearest possible evidence that the single writer thread — not the
  HTTP layer — is the real bottleneck, exactly as the architecture always
  implied but had never been measured directly until now.
- **Testing-methodology correction recorded, not a regression**: `cargo
  tree --no-default-features` now shows tokio/axum/jsonwebtoken because
  they're legitimate dev-dependencies for the test suite (`jsonwebtoken`
  and `futures-util` were added to `[dev-dependencies]` alongside the
  already-present `reqwest`), and `cargo tree` includes dev-dependency
  edges by default. The correct check for "does the default *library*
  build depend on tokio" is `cargo tree --no-default-features --edges
  normal`, confirmed empty throughout. Recorded here so a future session
  doesn't mistake the unfiltered `cargo tree` output for a real problem.
- `PROGRESS.md`'s `## M5 — API / server [DONE]` entry (full benchmark
  table + honest read) and this file's M5.a-d task-breakdown sections +
  Current status update, both written in this same session.
- Final verification: 205 unit (208 with `--features server`) + 11 crash
  + 4 `graph_locking` + 3 `graph_rebuild` + 2 `graph_mvcc` + 3
  `index_rebuild` + 1 `vector_mvcc` + 4 `queue_vacuum` + 2 `queue_mvcc` +
  25 `server_*` tests green, both with and without `--features server`;
  clippy/fmt clean both ways.
- **M0 through M5 — every milestone on CLAUDE.md's original roadmap — are
  now all DONE.** Nothing is in progress. The only explicitly deferred,
  not-yet-started work is the cross-domain "replaced stack" benchmark
  (CLAUDE.md §6) as its own separate future effort; anything beyond that
  is genuinely open and should be raised with the user directly in a
  future session, not assumed.

### 2026-07-06 — M5.a, M5.b, M5.c complete; xid-reuse-after-checkpoint bug found and fixed

- Planned M5 via the same process as M2-M4: three parallel research passes
  (Engine's full public API surface + `Send`/error shape; codebase
  module/error/test/bench conventions; external crate landscape for REST/
  JWT/metrics/sync-to-async bridging), a Plan agent producing a concrete
  checkpoint design, three confirmed decisions (writer-thread bridge over
  `Mutex<Engine>`; SSE over WebSockets for subscribe; verify-only stateless
  JWT, no login endpoint).
- **M5.a** — `Engine: Send` compile-time assertion, crate-level doc
  comment, transaction-boundary doc comments on `insert`/`get`/`delete`/
  `checkpoint`/`begin_with_isolation`/`commit`/`abort`, an unwrap/expect
  audit (confirmed clean — every non-test occurrence is either
  infallible-by-construction, an internal invariant, or an already-accepted
  RwLock-poisoning/thread-spawn-failure exception). `src/server/`
  (`engine_handle.rs`, `error.rs`, `mod.rs`) behind a new `server` Cargo
  feature — `EngineHandle` mirrors `index_worker.rs`'s spawn/channel/
  bounded-shutdown shape exactly, one dedicated OS thread owning `Engine`
  for its whole life.
- **M5.b** — axum/tokio brought in behind `server`; `src/server/`
  (`dto.rs`, `handlers.rs`, `router.rs`) plus `src/bin/unidb-server.rs`.
  Every mutating route wraps one `begin -> execute -> commit-or-abort`
  cycle; `/sql`/`/cypher` get atomic multi-statement transactions over
  HTTP for free via `execute_sql`'s existing `;`-separated-string support
  — no new engine code needed for that. `RowId`/`Edge`/`Event`/
  `IndexStatus` gained plain `serde::Serialize` derives (not feature-gated
  — `serde` is already an unconditional core dependency via `Literal`).
  Deliberately did **not** derive `Serialize` on `Literal`/`ExecResult`
  themselves: `Literal` already derives `Serialize`/`Deserialize`
  unconditionally for the catalog's on-disk RLS-policy blob, and changing
  that representation would be a breaking change to on-disk data — instead
  `server::dto::literal_to_json`/`exec_result_to_json` do the REST-facing
  conversion explicitly, reusing M4's `queue::payload::row_to_json`
  per-variant mapping. Manually smoke-tested end-to-end against a running
  `unidb-server`: SQL, raw CRUD, edges, index status, checkpoint, error
  mapping (404/409/400/500), multi-statement abort-rolls-back-the-row-data
  (though not the `CREATE TABLE` DDL itself — inherits M1's already-
  documented catalog-non-transactional gap), and graceful shutdown via
  real `SIGINT` (confirmed `EngineHandle::shutdown()` drains and joins,
  and a fresh `Engine::open` afterward sees everything committed).
- **Critical bug found via that same manual testing, fixed immediately as
  its own commit** (see the design note above and `PROGRESS.md`'s
  dedicated entry): checkpointing then reopening reset the xid counter to
  1 because `checkpoint::run`'s WAL truncation removes the very
  `WAL_TXN_BEGIN` records `recover_next_xid` depends on. Fixed by
  persisting `next_xid` in the control file (v2->v3 format bump, D3/D9,
  human sign-off confirmed before implementing) and resuming at
  `max(WAL-scan, control.next_xid)` on open. Regression test:
  `lib.rs::xid_counter_survives_reopen_after_checkpoint`.
- **M5.c** — `src/server/auth.rs` (verify-only HS256 JWT via
  `jsonwebtoken`'s `aws_lc_rs` backend, secret from `UNIDB_JWT_SECRET`;
  `require_jwt` middleware records `unidb_jwt_verify_seconds`),
  `src/server/sse.rs` (`GET /events/subscribe` — an `async-stream` loop
  polling `poll_events` on an interval and forwarding new events as SSE
  frames; explicit module-doc caveat that this is "server polls, pushes to
  client," not WAL-level push), `POST /tables/{table}/events` (new
  `handlers::post_enable_events`, needed since M5.b never exposed
  `Engine::enable_events` over HTTP), `GET /metrics` via `axum-prometheus`'s
  `PrometheusMetricLayer::pair()`. `router.rs` restructured into a
  `protected` sub-router (every data route, wrapped with
  `middleware::from_fn_with_state(jwt_config, auth::require_jwt)`) merged
  with a `public` sub-router (`/metrics` only, no auth layer), both under
  one top-level `PrometheusMetricLayer` so `/metrics` requests are counted
  too. `JwtConfig` is **not** part of `AppState` — `from_fn_with_state`
  accepts any `Clone + Send + Sync + 'static` state independent of the
  router's own state type, so passing `JwtConfig` directly to the auth
  layer (rather than threading it through `AppState`) keeps `AppState`
  focused on what every handler actually needs. Manually verified
  end-to-end (see Current status above): auth rejection matrix, SSE
  delivery + redelivery-until-ack, custom + auto-instrumented Prometheus
  metrics all real and correct against a running `unidb-server`.
- Verified throughout: `cargo build`/`test`/`clippy --all-targets -- -D
  warnings`/`fmt --all --check`, all clean **both** with and without
  `--features server`; `cargo tree --no-default-features | grep -i tokio`
  confirmed empty.
- Next: M5.d (hardening, automated test suite for M5.b/c, benchmarks,
  closeout).

### 2026-07-06 — M4 complete (all four checkpoints); M4 milestone DONE

- Planned M4 via the same rigorous process as M2/M3: three parallel
  research passes (WAL/checkpoint truncation logic, transaction-to-event
  boundary options, durable-offset-storage patterns), a Plan agent
  independently verifying the design against source, two
  user-confirmed decisions (queue-scoped benchmarks; Postgres-as-queue via
  `SELECT ... FOR UPDATE SKIP LOCKED` baseline), then implementation.
- **M4.a** — `src/queue/mod.rs`/`payload.rs` (new), `TableDef.
  events_enabled` + `Catalog::set_events_enabled`, `Engine::enable_events`,
  `next_event_seq` + `derive_next_event_seq`, `sql::executor::
  send_event_capture` wired into `exec_insert`/`exec_update`/
  `exec_delete`. Central finding: WAL-tailing is a dead end (no table
  identifier in WAL records, unconditional truncation) — events are
  copied into an ordinary `__events__` heap table at write time instead,
  exactly like `__edges__`. `ExecCtx` gained `next_event_seq: &'a mut u64`
  as a field — a deliberate, documented deviation from the approved plan
  (which favored an extra function argument) once the actual call graph
  (deeply nested private `exec_*` functions, not one top-level entry
  point) made the field approach clearly the better fit, matching
  `index_worker`'s existing precedent on the same struct.
- **M4.b** — `queue::find_consumer_offset`, `Engine::poll_events` (pure
  read, never writes), `Engine::ack_events` (the only write path to
  `__consumers__`, Kafka-style manual-commit split from `poll_events`).
- **M4.c** — `Engine::vacuum_events` (no-op with zero consumers; else
  reclaims `seq <= min(offsets)`; never called automatically) +
  `tests/queue_vacuum.rs`, including the milestone's central-claim proof
  (`wal_truncation_is_unaffected_by_consumer_lag`: a never-acking consumer
  survives five consecutive `checkpoint()` calls with zero data loss).
- **M4.d** — `tests/queue_mvcc.rs` (aborted event insert + aborted
  `ack_events`, both proven self-visible-then-invisible), a new two-table
  crash-recovery test in `tests/crash/main.rs` (first crash test spanning
  two tables in one incomplete user transaction — no new P-number needed),
  `benches/queue.rs` + a real Postgres SKIP LOCKED comparison (isolated
  `unidb_queue_bench` database, dropped after recording numbers).
  Benchmark headline: `poll_events`'s cost scales almost exactly linearly
  with `__events__`'s total size (confirmed, not assumed) since it has no
  predicate pushdown, while Postgres's partial index keeps its SKIP LOCKED
  dequeue flat regardless of table size — the clearest concrete argument
  yet for why `vacuum_events` matters as a latency lever, not just a
  storage one.
- Final state: 203 unit + 11 crash + 4 `graph_locking` + 3 `graph_rebuild`
  + 2 `graph_mvcc` + 3 `index_rebuild` + 1 `vector_mvcc` + 4
  `queue_vacuum` + 2 `queue_mvcc` (233 total) tests green, clippy/fmt
  clean, release build OK. Committed directly to `main` across four
  checkpoint commits (no feature branch — the M3.a PR-branch experiment
  was not repeated this milestone, per the user's earlier "switch to main
  and continue" instruction), each pushed immediately after its own
  test/clippy/fmt pass. `PROGRESS.md`'s M4 entry and this file both
  updated in the final (M4.d) commit.
- M1, M2, M3, and M4 are now all DONE. Nothing is actively in progress —
  see "In progress" above for the two explicitly deferred next efforts
  (M5 planning; the cross-domain "replaced stack" benchmark).

### 2026-07-06 — M3.d complete; M3 milestone DONE

- Implemented all of M3.d: `tests/graph_rebuild.rs`, `tests/graph_mvcc.rs`
  (the single most important test in M3, per the plan), a real Postgres
  benchmark comparison (`unidb_graph_bench`, an isolated database created,
  measured, and dropped — no artifacts left behind), and the `PROGRESS.md`/
  `MEMORY.md` closeout.
- Ran the MVCC-correctness test with the same discipline M2.d established:
  confirmed the inserting transaction's self-visible view *before*
  aborting (proving the index really did have the stale entry, not a
  vacuous pass), then proved a fresh transaction's traversal *and* an
  equivalent Cypher query both correctly exclude the aborted edge. Simpler
  than M2's equivalent test: no poll-before-abort dance needed since
  `EdgeIndex` is synchronous.
- **Ran a real, non-mocked Postgres benchmark** and found a genuinely
  strong, honest result worth highlighting: the batch-latch adjacency
  scan (M3.b) lands within ~1.6x of Postgres at 10,000 edges (930µs vs
  568µs) and is essentially tied at 1,000 edges (94.3µs vs 98µs) — while
  the pre-optimization naive scan would have lost by 9–16x. INSERT
  throughput still lags Postgres by ~35x, but that's the same pre-existing
  per-statement fsync gap M1/M2 already documented, not anything
  graph-specific — reported honestly rather than either hidden or
  conflated with a graph-specific weakness.
- **Final state:** 182 unit tests + 10 crash-harness tests + 4
  `graph_locking` + 3 `graph_rebuild` + 2 `graph_mvcc` + 3 `index_rebuild`
  + 1 `vector_mvcc` (205 total) green, `cargo clippy --all-targets -- -D
  warnings` clean, `cargo fmt --all --check` clean, `cargo build --release`
  succeeds.
- **M3 milestone is DONE.** All four checkpoints (M3.a/b/c/d) complete,
  benchmarked, and committed. Two things were found and confirmed *not* to
  need new code during implementation (no `RecordKind::GraphEdge` variant,
  no `ExecCtx` field for `edge_index`) rather than being built speculatively
  and left unused — both documented as design notes with the reasoning
  that ruled them out, not just asserted.
- **Next:** M4 planning (event queue) has not started — this session ended
  with M3 fully closed out, no M4 work begun.

### 2026-07-06 — M3.c complete (Cypher subset)

- Implemented all of M3.c: `src/graph/logical.rs` (`CypherQuery`,
  `ReturnItem`), `src/graph/parser.rs` (hand-rolled tokenizer +
  recursive-descent parser, no external crate — confirmed none exists
  during M3 planning), `src/graph/executor.rs` (reuses
  `sql::executor::predicate_matches`/`eval_expr` verbatim after promoting
  them to `pub(crate)`), `Engine::execute_cypher`.
- One real design deviation from the plan's literal sketch, resolved
  deliberately: `graph::executor::execute` takes `edge_index` as an
  explicit extra argument rather than a new `ExecCtx` field, keeping
  `sql::executor::ExecCtx` exactly the storage/transaction infra M1/M2
  already built — see the design note above.
- `MATCH (a)-[:TYPE]->(b) WHERE ... RETURN ...` round-trips end-to-end:
  a `from_id = <literal>` predicate routes through the M3.a/M3.b edge-list
  index + batch-latch resolver, everything else falls back to a full
  `__edges__` scan, and both paths apply the identical `:TYPE`+`WHERE`
  predicate through one shared `predicate_matches` call — no special
  casing for which path a candidate came from.
- **Final state:** 182 unit tests + 10 crash-harness tests + 4
  `graph_locking` + 3 `index_rebuild` + 1 `vector_mvcc` (200 total) green,
  `cargo clippy --all-targets -- -D warnings` clean, `cargo fmt --all
  --check` clean, `cargo build --release` succeeds.
- **Next:** M3.d — `tests/graph_rebuild.rs`, `tests/graph_mvcc.rs` (the
  aborted-edge MVCC-correctness test), the Postgres adjacency-table
  benchmark comparison, and M3 milestone closeout.

### 2026-07-06 — M3.a and M3.b complete

- M3 (graph) planning: three parallel research passes (lockmgr/catalog/heap
  reuse, SQL-layer extension points, Cypher-parser crate landscape) plus a
  Plan agent, confirmed against the actual source rather than assumed.
  Two decisions confirmed with the user: opaque `i64` node IDs only (no
  property-graph joins), and Postgres with an indexed adjacency-list table
  as the M3 benchmark baseline (mirroring M2's pgvector precedent).
- **M3.a — edge storage foundation**: graph edges stored as ordinary rows
  in a synthetic `__edges__` system table, auto-created at `Engine::open()`
  — zero new storage-layer code, full MVCC/WAL/crash-recovery/SQL-query
  ability for free. Synchronous in-memory edge-list index (no async worker
  — unlike M2, a `HashMap` insert doesn't need one). Committed as PR #1
  (`m3-graph-edge-storage` branch, merged via GitHub).
- **M3.b — locking verification + batch-latch**: confirmed, via tests not
  just code inspection, that per-edge locking needs zero new code
  (`RecordId::row`'s lock key is already globally unique across every
  table — no `RecordKind::GraphEdge` variant needed). Found and fixed a
  test-writing mistake along the way: `heap.rs::delete`'s two distinct
  conflict checks intentionally share one `WriteConflict` error shape, so
  a test trying to distinguish them by variant was wrong, not the code.
  Benchmarked the batch-latch adjacency scan honestly (not assumed) and
  found a real, large win: ~9.3–9.7x faster than naive per-candidate
  resolution at 1,000/10,000-edge hot hubs, tracking the measured
  edges-per-page ratio closely.
- **Workflow note**: this session's PR request surfaced that `main` had no
  feature-branch workflow all session (M0–M2 were committed directly to
  `main`). Resolved by creating `m3-graph-edge-storage` off `main` for
  M3.a, which the user reviewed and merged via GitHub's UI (PR #1) — local
  `main` was then fast-forwarded to match. M3.b continued as a direct
  commit to `main`, per explicit user instruction to resume the established
  pattern.
- **Final state:** 168 unit tests + 10 crash-harness tests + 4
  `graph_locking` + 3 `index_rebuild` + 1 `vector_mvcc` (186 total) green,
  `cargo clippy --all-targets -- -D warnings` clean, `cargo fmt --all
  --check` clean, `cargo build --release` succeeds.
- **Next:** M3.c — the Cypher subset (hand-rolled parser, `Engine::
  execute_cypher`, reusing `predicate_matches`/`eval_expr` after promoting
  them to `pub(crate)`).

### 2026-07-06 — M2.d complete; M2 milestone DONE

- Implemented all of M2.d: `Expr::Near` + parser support (zero grammar
  changes needed — `NEAR(...)` parses as an ordinary `SqlExpr::Function`),
  `exec_select_near`'s over-fetch-then-filter execution, `tests/
  index_rebuild.rs`, `tests/vector_mvcc.rs`, `benches/vector.rs`.
- **Found and fixed a real bug while wiring up `NEAR`, caught by the
  benchmark/integration tests themselves failing, not by inspection**:
  `MarkReady` on a column that had never received a single `Upsert` (the
  common case — `CREATE INDEX` on a table, then insert afterward) used to
  silently no-op, permanently stranding the index in `Building`. Root
  cause: the handler only updated an *existing* map entry; `Upsert`-driven
  entry creation always starts `Building` and nothing ever flipped a
  never-backfilled column to `Ready`. Fixed by giving `MarkReady` the
  `IndexKind` it needs to create an already-`Ready` empty entry.
- Ran the M2.d plan's explicitly-called-out "single most important test in
  M2": `tests/vector_mvcc.rs`'s aborted-insert test, using a deterministic
  poll-until-confirmed pattern (the inserting transaction's own
  self-visible `NEAR` query) rather than a timing-dependent sleep, per the
  plan's own caution against exactly that kind of flakiness.
- **Ran a real, non-mocked Postgres + pgvector benchmark**, not an
  estimate: `brew install pgvector` locally, an isolated `unidb_bench`
  database (dropped after recording numbers, no artifacts left behind),
  matching INSERT/`NEAR`-equivalent methodology against unidb's own
  `benches/vector.rs`. Recorded honestly in `PROGRESS.md`: unidb is far
  behind pgvector in absolute terms, and the writeup explains why (M1's
  already-known per-statement fsync cost, plus `instant-distance`'s
  full-rebuild-per-upsert cost measurably showing up even at 200 rows) —
  not flattered, per CLAUDE.md §6.
- **Final state:** 158 unit tests + 10 crash-harness tests + 3
  `index_rebuild` tests + 1 `vector_mvcc` test (172 total) green, `cargo
  clippy --all-targets -- -D warnings` clean, `cargo fmt --all --check`
  clean, `cargo build --release` succeeds.
- **M2 milestone is DONE.** All four checkpoints (M2.a/b/c/d) complete,
  benchmarked, and committed. Two design corrections were found and fixed
  during implementation rather than silently worked around: the
  `instant-distance` incremental-insert assumption (M2.b) and this
  session's `MarkReady` bug (M2.d) — both documented as design notes, not
  swept under the rug.
- **Next:** M3 planning (graph) has not started — this session ended with
  M2 fully closed out, no M3 work begun.

### 2026-07-06 — M2.c checkpoint complete (full-text index + CREATE INDEX)

- Implemented all of M2.c per the approved plan
  (`/Users/sagarmahamuni/.claude/plans/misty-hugging-brook.md`): `src/fulltext.rs`
  (`InvertedIndex`), generalized `index_worker.rs` to a `FullText` variant,
  `LogicalPlan::CreateIndex` + parser support, `exec_create_index` in
  `sql/executor.rs` with immediate backfill.
- **One real grammar detail found and documented, not guessed**: `sqlparser`
  0.62.0's `CREATE INDEX` only recognizes `USING <type>` *before* the
  column list, not after — the initial test SQL (`... (col) USING HNSW`)
  failed with `using: None` until read directly from `parse_create_index`'s
  source and corrected to `... USING HNSW (col)`.
- **One real latent gap found and fixed while building this, not left
  behind**: M2.b's rebuild-on-open only ever scanned `IndexKind::Hnsw`
  columns, so a `FullText`-indexed table would have silently lost its index
  on every engine reopen. Generalized the rebuild function
  (`rebuild_vector_indexes` → `rebuild_secondary_indexes`) to scan any
  indexed column, sharing the same `build_indexed_columns` helper newly
  factored out of the executor for exactly this purpose.
- Confirmed by design, not by accident: `CREATE INDEX` backfills
  immediately (scans and enqueues right there in the executor), while
  M2.b's `Engine::set_column_index` Rust API still only populates on next
  reopen — two different entry points with two different eagerness
  contracts, both intentional and both documented.
- **Final state:** 148 unit tests + 10 crash-harness tests green, `cargo
  clippy --all-targets -- -D warnings` clean, `cargo fmt --all --check`
  clean, `cargo build --release` succeeds.
- **Next:** M2.d — `NEAR` operator (`Expr::Near`, over-fetch-then-filter in
  `exec_select`), `tests/index_rebuild.rs` and `tests/vector_mvcc.rs` (the
  MVCC-rollback-correctness test — the single most important test in M2 per
  the plan), benchmarks with the Postgres+pgvector comparison, M2 milestone
  closeout in `PROGRESS.md`.

### 2026-07-06 — M2.b checkpoint complete (background indexing worker)

- Implemented all of M2.b per the approved plan
  (`/Users/sagarmahamuni/.claude/plans/misty-hugging-brook.md`): `src/vector.rs`
  (`VectorIndex` wrapping `instant-distance`), `src/index_worker.rs` (the
  engine's first background thread), rebuild-on-open + live-upsert wiring
  through `lib.rs`/`sql/executor.rs`, `Engine`'s `Drop` impl.
- **One real design correction found and fixed, not silently absorbed**:
  the plan assumed `instant-distance` supports native incremental insertion.
  Checked against the vendored 0.6.1 source before writing any code against
  it — it doesn't; `Builder::build` only does full-rebuild construction.
  Corrected `VectorIndex` to buffer points and rebuild the whole graph per
  upsert, documented as a design note and a tracked tech-debt item (M2.d's
  benchmark table is where this gets quantified honestly, not assumed away).
- Pulled one small primitive (`Catalog::set_column_index`/
  `Engine::set_column_index`) forward from its originally-planned M2.c slot,
  narrowly justified: M2.b's own tests needed a way to mark a column
  indexed to prove the worker pipeline end-to-end, and this is exactly the
  catalog-persistence call `CREATE INDEX` was always going to make
  internally — not a competing mechanism, and it deliberately does *not*
  backfill (that's still M2.c's job).
- Confirmed the plan's core risk-mitigation choice held up in practice: the
  worker thread's only state is `Arc<RwLock<HashMap<(table,column),
  IndexEntry>>>`, built purely from channel messages — it never received a
  `BufferPool`/`Wal`/`Heap` handle anywhere in the implementation.
- Flagged one new tech-debt item, parallel to M1's "no vacuum" gap: no
  index cleanup on UPDATE (old vector values under dead `RowId`s
  accumulate forever) — a space leak, not a correctness bug, since stale
  candidates resolve to `NoVisibleVersion` at read time.
- **Final state:** 131 unit tests + 10 crash-harness tests green, `cargo
  clippy --all-targets -- -D warnings` clean, `cargo fmt --all --check`
  clean, `cargo build --release` succeeds.
- **Next:** M2.c — full-text index (`src/fulltext.rs`) + explicit
  `CREATE INDEX ... USING HNSW|FULLTEXT` SQL surface, generalizing the
  worker's `SecondaryIndex` enum to a second variant and reusing
  `set_column_index` from the executor side this time.

### 2026-07-06 — M2.a checkpoint complete (VECTOR(n) foundation)

- Implemented all of M2.a per the approved plan
  (`/Users/sagarmahamuni/.claude/plans/misty-hugging-brook.md`):
  `ColumnType::Vector(u32)` + `IndexKind` in `catalog.rs`; row encoding tag
  5 (`[dim:4 LE][f32*dim]`) in `sql/executor.rs`'s `coerce_value`/
  `encode_row`/`decode_row`; `Literal::Vector(Vec<f32>)` in
  `sql/logical.rs`; parser support for `VECTOR(n)` (via `DataType::Custom`)
  and `[..]` array literals (via `SqlExpr::Array`) in `sql/parser.rs`.
- No design deviations from the plan — both `sqlparser` internals
  (`DataType::Custom` fallback, unconditional `SqlExpr::Array` parsing under
  `GenericDialect`) were confirmed against the vendored 0.62.0 source ahead
  of time in the plan, and held up exactly as expected during
  implementation.
- Dimension validation is deliberately redundant across three layers
  (parser rejects `n=0`, executor's `coerce_value` checks INSERT/UPDATE
  literals, `decode_row` re-checks stored bytes on every read) — see design
  note above for why each guards a distinct failure mode.
- Added end-to-end SQL-level tests (`execute_sql_vector_round_trip`,
  `execute_sql_vector_dimension_mismatch_rejected` in `lib.rs`) on top of
  the parser/executor unit tests, confirming the feature works through the
  real `Engine::execute_sql` path, not just in isolated unit tests.
- **Final state:** 121 unit tests + 10 crash-harness tests green, `cargo
  clippy --all-targets -- -D warnings` clean, `cargo fmt --all --check`
  clean.
- **Next:** M2.b — the background indexing worker (`src/index_worker.rs`,
  `src/vector.rs` wrapping `instant-distance`). This is M2's highest-risk
  checkpoint: the engine's first background thread, which must never touch
  `BufferPool`/`Wal`/`Heap`. See the plan file's tasks 6–12.

### 2026-07-06 — M1.d complete; M1 milestone DONE

- Added the combined crash+MVCC property test (`tests/crash/main.rs`): a
  small self-contained LCG (deliberately not a new `rand` dependency, since
  this is test-only and reproducibility just needs a fixed seed) drives
  random transaction sequences across 6 seeds with random crash points,
  including true mid-transaction crashes (no commit/abort call at all).
  Passed first try — no bugs found by this specific test, a genuine "the
  invariant holds" result, not just "test not written yet."
- Extended `benches/load.rs` with a `contention` benchmark group measuring
  SI's abort-on-conflict + retry cost, not just uncontended CRUD.
- Ran the full M1 benchmark suite (`--sample-size 10`, not the default 100,
  since each sample involves real fsyncs and the default would have taken
  well over an hour based on M0's timing) and recorded the table in
  `PROGRESS.md`.
- **Found a real, previously-unnoticed bug while benchmarking, not a
  pre-planned test**: point `SELECT`'s cost went from 855ns (M0) to 3.05ms
  (M1) — far more than the ~2x expected from transaction-wrapper overhead.
  Root cause: `TransactionManager::commit()` fsyncs unconditionally, even
  for read-only transactions that wrote nothing. Documented as a design
  note, recorded in `PROGRESS.md`, and left as an open question for
  deliberate fix-now-vs-defer decision rather than silently patched in
  passing — this touches a path CLAUDE.md's own conventions would want
  reviewed as a real change, not folded into an unrelated commit.
- INSERT/UPDATE landed at ~2x M0's cost, exactly as expected (each
  single-statement-per-transaction op now pays both the existing
  per-statement mini-txn fsync and a new per-transaction commit fsync) —
  confirmed this is inherent to the benchmark's "worst case: no batching"
  design, not a surprise regression.
- **Final state:** 112 unit tests + 10 crash-harness tests (P1–P9 + the
  new property test) green, `cargo clippy --all-targets -- -D warnings`
  clean, `cargo fmt --all --check` clean, `cargo build --release` succeeds.
- **M1 milestone is DONE.** All four checkpoints (M1.a/b/c/d) complete,
  benchmarked, and committed. Two open, human-decidable items carried
  forward rather than resolved unilaterally: the read-only-txn fsync fix,
  and whether catalog DDL needs transactionality before M2.
- **Next:** M2 planning (vector search) has not started — this session
  ended with M1 fully closed out, no M2 work begun.

### 2026-07-06 — M1.c checkpoint complete (catalog + SQL subset)

- Implemented all of M1.c: `catalog.rs` (schema + page-list persistence,
  `serde_json`-encoded, not MVCC-versioned), `sql/logical.rs` (LogicalPlan/
  Expr + `apply_rls`), `sql/parser.rs` (wraps `sqlparser` 0.62.0), `sql/
  executor.rs` (row-at-a-time execution, hand-rolled row encoding, no
  separate physical-plan IR), `Engine::execute_sql`/`set_rls_policy`.
- Fixed a real pre-existing bug while building table storage: `Heap`'s
  in-memory-only page list (flagged as tech debt since M0) would have made
  `scan()` silently return nothing for existing rows after every reopen.
  Now persisted via `TableDef.pages` in the catalog; `Heap::from_pages`/
  `page_ids()` let the executor reconstruct/detect-growth per statement.
- Discovered and worked around a `sqlparser` `GenericDialect` precedence
  surprise: `->`/`->>` bind looser than `=`, opposite of the initial
  assumption — documented, not treated as a bug.
- Two scope simplifications made and explicitly flagged rather than silently
  dropped: catalog DDL is not transactional/MVCC-versioned; RC's
  EvalPlanQual re-evaluation path remains unimplemented even though it's now
  unblocked (both noted in Open questions above for future work).
- **Final state:** 112 unit tests + 9 crash-harness tests (P1–P9) green,
  `cargo clippy --all-targets -- -D warnings` clean, `cargo fmt --all --check`
  clean, `cargo build --release` succeeds.
- **Next:** M1.d — combined crash+MVCC property test, extend
  `benches/load.rs`, fill in M1's benchmark table, close out the milestone.

### 2026-07-06 — M1.b checkpoint complete (SI abort-on-conflict)

- Implemented all of M1.b: `lockmgr.rs` (write-write conflict tracking, no
  wait queue per D12), wired into `Heap::update`/`delete`, `Engine`/
  `TransactionManager` now own and thread a `LockManager` alongside
  `pool`/`wal`/`heap`, crash test P9 (crash mid-undo of an already-aborting
  transaction).
- One planned mechanism turned out to be unnecessary: the "commit-time
  first-committer-wins recheck" is subsumed by holding locks for a
  transaction's entire lifetime (released only at commit/abort) — analyzed
  and documented as a design note rather than building redundant code that
  would never actually fire in this single-threaded engine.
- Added 3 hand-written interleaved-transaction tests demonstrating SI
  abort-on-conflict end-to-end: immediate abort on write-write conflict,
  lock release on commit, lock release on abort.
- **Final state:** 80 unit tests + 9 crash-harness tests (P1–P9) green,
  `cargo clippy --all-targets -- -D warnings` clean, `cargo fmt --all --check`
  clean, `cargo build --release` succeeds.
- **Next:** M1.c — catalog (`catalog.rs`) + SQL subset (`src/sql/`), with
  RC's re-evaluation path landing inside the UPDATE/DELETE executor and
  RLS's AND-rewrite landing in the logical planner.

### 2026-07-06 — M1.a checkpoint complete (MVCC core)

- Implemented all of M1.a per the approved plan
  (`/Users/sagarmahamuni/.claude/plans/misty-hugging-brook.md`): tuple header
  extension, control-file catalog_root field, WAL user-txn records, MVCC
  visibility logic, transaction manager, MVCC-aware heap rewrite, recovery's
  user-txn undo pass, on_read/on_write seam, P6/P7 crash tests.
- Two design deviations from the original plan discovered during
  implementation and corrected (see design notes above): (1) abort requires
  immediate physical undo, not something deferrable to M1.b; (2) no
  version-chain walk in `Heap::get` — no cross-statement RowId stability.
- Fixed a real bug introduced mid-session: `recovery.rs`'s `redo_record`/
  `undo_record` still assumed M0's WAL_INSERT/WAL_UPDATE payload semantics
  (bare payload / full replacement) after `heap.rs` changed what those
  records actually carry (versioned-insert encoding / bare xmax value).
  Fixed by decoding the new payload shapes explicitly.
- Also closed out M0 in this session: ran `cargo bench --release` (some
  benchmarks took several minutes each due to per-op fsync), recorded the
  metrics table in `PROGRESS.md` with a lightweight SQLite CLI/Python-driver
  baseline comparison, and fixed pre-existing repo-wide `cargo fmt` drift
  that predated this session (confirmed via `git stash` before touching it).
- **Final state:** 71 unit tests + 8 crash-harness tests (P1–P7) green,
  `cargo clippy --all-targets -- -D warnings` clean, `cargo fmt --all --check`
  clean, `cargo build --release` succeeds.
- **Next:** M1.b — lock manager, SI abort-on-conflict (built and tested
  before RC's re-evaluation path, per D12), crash test P9.

### 2026-07-06 — M0 implementation (Tasks 1–10)

- Created all M0 source modules from scratch (Tasks 1–10).
- Fixed D5 enforcement: `write_page` is in-memory only (no D5 check); D5 is
  enforced at `flush_page()` and `find_victim()` eviction.
- Fixed `mmap.rs` `unsafe` isolation: crate uses `#![deny(unsafe_code)]`, mmap
  module uses `#![allow(unsafe_code)]`.
- Fixed WAL BufWriter flush ordering: tests that scan the WAL now commit (fsync)
  before scanning so records are durable on disk.
- **Final state:** `cargo clippy -- -D warnings` clean, 30 unit tests + 6 crash
  harness tests all green.
- **Next:** Run benchmarks (`cargo bench --release`), record results in
  `PROGRESS.md`, mark M0 done.

### 2026-07-13 — Item 26: event queue at scale

- **Q1:** Added `DiskBTree::search_range_limit` in `src/btree_index.rs`; wired
  `ensure_event_seq_index` (mirrors `ensure_edge_index`, migration-safe) in
  `Engine::open`; `poll_events` / `poll_events_after` rewritten to use index +
  MVCC re-check; `ExecCtx.event_seq_index_meta` threads meta page into
  `send_event_capture` for index insert on every event append.
- **Q2:** `EventWake` (condvar + generation counter) added to `src/lib.rs`;
  `Engine::commit` notifies after `sync_up_to` (P5.e clean); dispatcher builder
  accepts `event_wake`; SSE route uses `wait_event_commit` loop.
- **Q3:** `vacuum_events` collects `(row_id, seq)` pairs and removes seq index
  entries after heap delete — no retention pinning.
- **Crash point P30:** seq index torn mid-append; crash harness now 32/32.
- **Bench:** `benches/poll_events.rs` + `[[bench]] poll_events` in `Cargo.toml`;
  flat-latency proven 10k→100k→300k rows.
- Fixed clippy `too_many_arguments` on `ensure_event_seq_index` with
  `#[allow(clippy::too_many_arguments)]` (mirrors `ensure_edge_index`).
- Docs: `engine_design.md` §6.2/§6.3/tech-debt corrected; spec + backlog index
  row 26 flipped to SHIPPED; PROGRESS.md entry with bench numbers.
- **Gates:** cargo test --workspace --features server green (385 + 32 crash);
  clippy/fmt clean; conc-matrix 28/28.
- **Next:** await PR review — do not merge.

### 2026-07-13 — Item 29: subscription CDC — canonical envelope, before/after, format adapters, lag observability

- **C1 (before/after capture):** `Event` struct gained `before: Option<Value>`,
  `after: Option<Value>`, `ts_ms: i64` (skip-if-none). `send_event_capture`
  signature changed to `(table_def, op, before: Option<&[Literal]>,
  after: Option<&[Literal]>, ctx)`. UPDATE now clones `before_row` prior to
  `set_column`; INSERT passes `(None, Some(&coerced))`; DELETE `(Some(&row), None)`.
  Canonical envelope stored in `__events__.payload`:
  `{payload:<compat>, before, after, ts_ms, source:{seq,txId,table,schema}}`.
  Back-compat: `payload` key contains the old flat row; `resolve_event_candidates`
  detects old events (no "payload" key) and reads them transparently.
- **C2 (format adapters):** New file `src/server/event_format.rs` —
  `format_event(event, format)` dispatching to `format_debezium` /
  `format_supabase` / native (default). `SubscribeParams.format` field added
  to SSE route. 7 unit tests covering all three ops × all three formats.
- **C3 (lag observability):** `SubscriptionLagEntry` struct + `subscription_lag`
  field in `EngineStats`; `subscription_lag_stats()` method on `Engine` using
  `read_snapshot`, `DiskBTree::max_entry()` (O(log n)), `search_range_limit`
  for oldest unconsumed ts_ms. `unidb_catalog.subscription_lag` added to
  `information_schema.rs` (schema + `subscription_lag_rows()` with
  pool+snapshot context, special-cased in `query_exec.rs`). Prometheus gauges:
  `unidb_subscription_lag_events{consumer}` + `unidb_subscription_lag_seconds{consumer}`
  in `router.rs` `publish_engine_metrics`.
- **C4 (docs):** `engine_access_guide.md` §8 updated — §8.1 new fields + ts_ms;
  §8.2 wire formats (native/debezium/supabase examples); §8.3 Consuming (old §8.2,
  + format note); §8.4 Replay/vacuum (old §8.3); §8.5 dispatcher (old §8.4);
  §8.6 lag observability (virtual relation, /stats, Prometheus, alert guidance).
- **Tests:** 3 existing CDC tests updated (use `env["payload"]["col"]`);
  3 new tests: `cdc_c1_before_after_images_per_op`,
  `cdc_c3_subscription_lag_virtual_relation`,
  `cdc_c3_stats_subscription_lag_matches_virtual_relation`.
- **Dispatch crate:** `unidb-dispatch` test helpers in `filter.rs` / `sink.rs`
  updated for new Event fields — all dispatch tests green.
- **Gates:** `cargo test --workspace --features server` all green; crash 33/33
  (unchanged); `clippy --workspace --all-targets -D warnings` clean; `fmt` clean.
  No FORMAT_VERSION bump, no WAL record type changes, no §3 decision reopened.
- **Docs / tracking:** `29_subscription_cdc_envelope_lag.md` → SHIPPED
  (acceptance checkboxes filled); `backlog_index.md` row 29 → SHIPPED;
  `PROGRESS.md` item 29 entry added; `README.md` status line + milestone table
  updated.
- **Next:** push branch, open PR referencing spec + items 20/26/18/21 —
  STOP for review, do not merge.

### 2026-07-06 — Project initialization
- Architecture design doc reviewed; six foundational gaps identified and resolved.
- Isolation decided: RC default / RR available / SSI seam now (D10–D12).
- Scope adjusted: single-file for M0 (D6); benchmark the replaced stack (§6).
- `CLAUDE.md`, `PROGRESS.md`, `MEMORY.md` created.

### 2026-07-14 — FK row-level enforcement (item 36), branch `36-foreign-key-row-enforcement`

**Problem solved:** FK enforcement was table-existence-only (M11 deliberate scope).
Dangling child references were silently accepted; parent deletes were never blocked.

**Implementation:**

- `src/error.rs` — `ForeignKeyViolation` gained `column: Option<String>` + `value:
  Option<String>` for row-level error context; `fk_violation_msg` helper added.
- `src/lockmgr.rs` — `RecordKind::FkKey` phantom lock + `RecordId::fk_key(hash)`.
  Keyed by `hash(parent_table, ref_col, value)`; acquired Exclusive by both child
  inserter (before snapshot) and parent deleter (before RESTRICT scan); held through
  commit via `release_all`. Prevents parent-delete / child-insert race.
- `src/sql/executor.rs` — ~400 lines of FK helpers:
  - `acquire_fk_key_locks` — child-side exclusive FkKey lock, column-level and
    single-column table-level FKs, before snapshot
  - `acquire_fk_key_locks_parent` — parent-side FkKey lock on PK values, before
    RESTRICT scan
  - `enforce_fk_rows_exist` — child INSERT/UPDATE: calls `check_fk_parent_exists`
    per FK column; O(log n) via `unique_index_root`; heap fallback for composite
  - `enforce_fk_restrict` — parent DELETE/UPDATE: scans catalog for referencing
    children; uses child secondary BTree if available, heap fallback otherwise
  - `table_has_fk_children` — quick catalog gate to skip RESTRICT overhead
  - `resolve_fk_ref_col` — resolves explicit or inferred parent column name
  - `exec_insert`, `exec_update`, `exec_delete` updated: FkKey locks acquired
    before snapshot; FK enforcement called after unique enforcement
- `src/catalog.rs` — `ForeignKeyRef` doc updated from "informational" to enforced
- `tests/constraints.rs` — 2 existing tests updated; 9 new tests:
  `fk_row_existence_missing_parent_rejected`, `fk_row_existence_valid_parent_accepted`,
  `fk_null_column_not_checked`, `fk_same_txn_parent_then_child_accepted`,
  `fk_restrict_blocks_parent_delete_with_children`,
  `fk_restrict_allows_parent_delete_no_children`,
  `fk_table_level_constraint_enforced`,
  `fk_update_to_missing_parent_rejected`,
  `fk_child_insert_throughput_is_flat`
- `benches/conc_matrix.rs` — `w_fk_delete_insert_race` + 2 cells (toggle off/on)

**Gates:** fmt ✅, clippy ✅, workspace tests ✅, crash 37/37 ✅, constraints 27/27 ✅,
`fk-delete-insert-race` CONC_REPEATS=10: **10/10 PASS** (both toggles).
No FORMAT_VERSION bump. Commit `b1b0c33`.

**Docs updated:** `36_foreign_key_row_enforcement.md` → SHIPPED; `backlog_index.md`
row 36 → ✅ SHIPPED; `docs/engine_access_guide.md` §1 limitations + §9 FK enforcement
note updated; `README.md` item 36 row added; `PROGRESS.md` item 36 entry appended.

**Limitations:** `ON DELETE CASCADE/SET NULL` not implemented (RESTRICT only).
Composite FK without secondary index on child FK column uses O(n) heap scan for
RESTRICT (documented). No FORMAT_VERSION bump; no §3 decision change.

**Next up:** Open PR #103. After merge, identify next backlog item (check item 37+).

### 2026-07-14 — Guide: new §11 "Configuration & performance tuning" in the PDF, branch `claude/config-options-docs-rn6phg`

**Docs-only, no code/behavior change.** User asked for a section in
`docs/design/unidb_engine_architecture.pdf` covering every engine/server
config option that can be tuned for performance, with purpose + impact.

- `docs/design/unidb_engine_architecture.html` — new §11 "Configuration &
  performance tuning" inserted after §10 (Performance), with 5 subsections
  (11.1 memory/storage, 11.2 WAL/durability, 11.3 query execution/concurrency,
  11.4 vacuum, 11.5 REST server timeouts) tabulating every `UNIDB_*` env var
  found in source (`lib.rs`, `sql/parallel_scan.rs`, `sql/sort.rs`,
  `sql/plan.rs`, `wal.rs`, `server/mod.rs`, `server/router.rs`,
  `bin/unidb-server.rs`): default, purpose, and measured/architectural
  performance impact (e.g. the buffer-pool mmap-vs-shared_buffers distinction
  and its measured collapse-to-1.2k-rows/s story from the 2026-07-14
  buffer-pool-bump entry above). Old §11–§14 renumbered to §12–§15 throughout
  (TOC, anchors, and the handful of in-body "Section N" cross-references);
  verified via rendered screenshots, no orphaned anchors.
- `docs/design/unidb_engine_architecture.pdf` regenerated via
  `render_pdf.mjs` (headless Chrome needed `--no-sandbox` in this container —
  not committed, just a local invocation flag).
- `docs/design/unidb_engine_architecture_context.md` and
  `docs/design/design_index.md` updated to describe the new section and its
  renumbering (coverage snapshot, §-reference fixes, source-material list).

**Next up:** none pending from this session — resume backlog item 37+ per the
prior entry.
