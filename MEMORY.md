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

- **2026-07-24 (second session entry) — item 114 Step-0 probe + item 117 filed (checkpoint/background-writer D5 race).**
  Built `UNIDB_BENCH=item114_step0` in `benches/decompose.rs`: ladder rungs W1–W4 at
  MM_SIZES in BOTH configs on one commit — async HNSW worker (07-23/shipping) vs sync
  fallback (07-21) — so Δevent/Δvector attribution is a same-commit A/B, not a
  cross-report inference. `mm_ladder_point` refactored to `mm_ladder_point_cfg(…,
  async_worker)`. **Smoke run found a real engine bug → item 117:** auto-checkpoint
  (60 s time trigger) fired inside a pre-grow commit and hard-errored "D5 violation on
  flush: page 689 LSN 86719 > durable WAL LSN 86718" — checkpoint snapshots the durable
  frontier once, then `flush_all` errors on any page a concurrent WAL writer dirties past
  it. NOT an item-107 regression, an item-107 *revelation*: autovacuum has always had the
  identical exposure (vacuum takes `write_serial`, `Engine::checkpoint` doesn't — verified
  by inspection), it just self-syncs promptly enough to keep the window narrow. Preferred
  fix (117 doc): flusher-side sync-up-to-pageLSN — one discipline covering all writers.
  Bench-side mitigation shipped: ladder points now run with auto-checkpoint OFF +
  drain-before-checkpoint at 50k boundaries (also de-flakes the full mmreport ladder).
  Cross-session coordination: #211 merged; #213 (Main: sensitivity sub-table +
  per-worktree COMPOSE_PROJECT_NAME) rebased onto, my duplicate compose fix dropped;
  numbering collision with #210 (owns 115/116) caught pre-push → filed as 117, index
  next-pointer → 118. Machine serialization queue honored (Main → items-115/116 → me).
  **Next: 100k Docker A/B run (item114_step0), results → PROGRESS + 114 doc, then the
  item-117 engine fix as its own PR (tag Main session for review).**

- **2026-07-24 — fix(hnsw): item 106 Unit 2a cold-path duplicate-rid bug (visited-bitset slack).**
  Three deterministic macOS failures on unmodified main `4c56740` (crash p17 NEAR top-5
  `[49,50,50,51,51]` after crash-reopen; `index_rebuild::near_on_index_built_over_empty_table`;
  `vec_distance::…ascending`). Root cause in PR #208's dense visited bitset: bounded at *word*
  granularity (`w < visited_bits.len()`), so the ≤63 slack bits of the `div_ceil(64)` allocation
  counted as in-range. A rid first seen slot-less went to the HashSet spill; a mid-search vec-cache
  fill could assign it a slack slot, and the re-encounter passed the unset bitset bit without
  consulting the HashSet → double-visit → duplicate rid in top-k. Fix (`search_layer_with_vec`):
  capture `bits_cap = num_slots()` at search start and bound at *slot* granularity — mid-search
  appends always get slots ≥ `bits_cap`, staying on the HashSet path they started on. Warm path
  unchanged by construction. Evidence: 3/3 failing tests → green; full suite + crash 54/54 green;
  fmt+clippy clean. Perf A/B same-machine back-to-back at ef=120: pre-fix 510.5/512.8 µs,
  with-fix 503.5/507.4 µs (neutral; recall 0.910 identical). NOTE: the 466 µs Unit 2a baseline
  did NOT reproduce today even pre-fix (~511 µs on unmodified code) — environment drift, tracked
  as a caveat, not a regression from this fix. Item 106 doc got a resolved known-issue banner.

- **2026-07-23 — Fresh full Docker bench RUN + PROMOTED as new MM_BASELINE; item 114 filed.**
  `docs/performance/report_20260723_124415.md` (main `0324dc5`, 84m 58s, canary quiet vs 07-21,
  conc matrix 32/32). **Item 107 validated in-record:** W4/W0 at 100k 96→**34.21×**, Δvector
  +17.55→**+3.31 ms/commit**, drain reported off-path (new table). CRUD wins: filtered 0.45→0.58×
  (beat item 109's ~0.50 prediction), non-HOT 0.65→0.85×, HOT 1.06→1.18×, COUNT 41→56×; Table 4
  at 100k 13.4→10.05 ms/txn. **New finding → item 114:** Δevent at 100k doubled (+4.08→+9.93
  ms/commit, now the dominant W4 rung) + the +3.31 Δvector commit-path residue — Step-0 =
  attribution A/B (worker CPU contention vs real event-path regression) before any lever.
  Note: item 106's NEAR gains are NOT in this report (mmreport doesn't measure NEAR — the
  standing Linux NEAR spot-check gap). **Next up:** item 106 Unit 3 (re-rank decode-pushdown,
  466→≤400 µs at ef=120) then Unit 2b; item 114 Step-0; 109 follow-ups; chips.

- **2026-07-22 (same session, after the docs audit) — MEMORY/PROGRESS roll-up + LESSONS.md.**
  Both files had grown past useful context size (MEMORY ~103k tokens, PROGRESS ~141k — together
  more than a full context window). Split into working set + verbatim archive:
  entries older than 2026-07-20 → `docs/history/MEMORY_ARCHIVE_2026-07.md` /
  `PROGRESS_ARCHIVE_2026-07.md` (headings intact, newest-first preserved; nothing deleted);
  PROGRESS gained an all-entries index table (127 entries, live/archive column). MEMORY now
  ~45 KB, PROGRESS ~96 KB. **New `LESSONS.md`** (35 standing rules swept from 500+ session
  entries: bench hygiene, evidence rules, engine invariants, tooling) — read every session per
  CLAUDE.md §0 step 2. Roll-up policy + thresholds added to §0.4; **new `scripts/lint_docs.sh`**
  (size thresholds, PROGRESS-reference resolution — caught and fixed 8 pre-existing paraphrased
  refs — archive pointers). Both lints green. Also fixed: `backlog_index.md` "next file" pointer
  113→114 (missed when item 113 was registered).

- **2026-07-22 docs audit session — full `docs/` + README staleness sweep and repair (no code changes).**
  Root cause found: content sections got patched at ship time but cross-cutting scaffolding didn't —
  23 backlog status corrections (index rows 107/109/110/111 were behind their files; 19 file headers
  behind their index rows), `engine_design.md` FORMAT_VERSION 8→12 + IVF/HNSW contradictions + module
  map + §12 gap registry, ops_runbook log-pruning claim (server DOES prune, `UNIDB_LOG_RETAIN_DAYS`=7)
  + new autovacuum section, access-guide/sql-reference "not supported" lists corrected (6 shipped
  features; CTEs and JOIN ON were wrongly denied; INCLUDE documented; verifier 33/33), README perf
  tables refreshed to the official 07-21 report, positioning.md brought to post-Phase-6 reality,
  documentation_index rebuilt by audience, `docs/performance/README.md` added (authoritative report =
  `report_20260721_035629.md`), orphan duplicate-ID backlog file renumbered → **item 113** (FK error
  direction, still live), empty `engine_internals_doc_prompt.md` given honest content.
  **New guard: `scripts/lint_backlog.sh`** — cross-checks every numbered file's Status header vs its
  registry row + orphan/duplicate-ID detection; clean pass (89 files). Run it before any docs push.

- **2026-07-22 session complete — items 110, 111 shipped + merged; item 112 filed (parked).**
  110 (#198): RLS+LIMIT crash — `current_user` was destroyed by the QuerySpec policy
  conversion's `Bool(true)` fallback (leak hazard in Bool-typechecking shapes, crash here);
  fixed by substituting at policy-injection time (`apply_rls(plan, catalog, user)`) + fallback
  now fails CLOSED (Null + warn); 5 count-asserted regression tests.
  111 (#199): information_schema.* needs no view grant; rows filtered per-caller ANY-privilege
  across all five views (Postgres semantics); unidb_catalog.* stays Z5 grant-gated; 5 tests.
  112 (#200): column-level grants scoped + deliberately parked; item-24 registry corrected —
  Z4's role-inheritance half had SHIPPED (transitive has_privilege, PR #166), only column
  grants were never built.
  Earlier same session: 105 (#190 selective bench + carry-forward), 92 (#191 NEAR ~900 µs),
  108 (#192/#193 env-drift proof + canary), 107 (#196 async HNSW activation + freshness gauge),
  109 (#197 page-cached resolution, warm filtered 3.0×).
  **Next up:** fresh full Docker bench on current main (first official record of item 107's
  ladder collapse; becomes new MM_BASELINE) · item 106 (vector ≤400 µs tier) · 109 follow-ups
  (one-shot fixed cost ~700 µs; Table-3 warm-median methodology decision) · chips (item-103
  LIMIT test variant, test-binary clippy cleanup). Parked: 112, item-19 CTE/window residue.

- **Item 109 — SHIPPED 2026-07-22; item 107 — SHIPPED + MERGED (#196) same day.**
  109: Step-0 refuted filed design (parallel resolution existed since items 45/54); real lever =
  per-candidate 8 KiB page-copy+CRC in get_visible (~1 µs × 5k candidates on ~25-50 pages). Fix:
  `get_visible_cached` single-page cache → **warm 3.0×** (973→323 µs native; 460 µs in-container).
  Docker Table-3 cert honest: 0.45→**0.50× one-shot** — bench times ONE cold execution (split:
  leaf 58 + resolve 901 + ~700 µs one-shot fixed cost) so warm wins can't appear there; both
  numbers in ceilings table; follow-ups: one-shot fixed cost, warm-median methodology question.
  107: item-67 worker existed but nothing spawned it (server + bench both took sync fallback —
  the W4/W0 96× measured that); EngineHandle::spawn now activates it; freshness contract (a)
  signed off (queue-depth gauge `unidb_hnsw_queue_depth`); bench drain-accounting added.
  Session hygiene lessons: NEVER run suites concurrently with a bench (self-inflicted the
  item-108 effect twice); compose runs can hang on leftover PG state — `docker compose down -v`
  before bench reruns. Item 110 (RLS+LIMIT, from user's PR #195 renumbered): fix + 5 tests
  AUTHORED in /tmp/unidb-110 (apply_rls injection-time substitution + fail-closed Null fallback),
  build/test/PR next.

> **Older Current-status entries (before 2026-07-22) live verbatim in
> [`docs/history/MEMORY_ARCHIVE_2026-07.md`](docs/history/MEMORY_ARCHIVE_2026-07.md)
> (rolled 2026-07-22 and 2026-07-24). Grep there for any dated entry; nothing
> was deleted.**

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

> **The completed M1–M5 task breakdowns were rolled into the archive on 2026-07-22.**

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

### 2026-07-24 — #211 (HNSW duplicate-rid fix) shipped+merged; item 114 Step-0 probe built; item 117 filed

Two work streams in one session (details in the two 07-24 Current-status entries):
(1) the item 106 Unit 2a visited-bitset slack bug — found via 3 deterministic NEAR
test failures, fixed at slot granularity, perf-neutral by same-machine A/B, merged
as #211. (2) item 114 Step-0 `item114_step0` bench mode built; its smoke run
surfaced the checkpoint-vs-background-writer D5 race → filed as item 117 (number
chosen after checking #210's claimed 115/116). Heavy cross-session machine
serialization this session (Main → items-115/116 → me) — two near-collisions:
my image build overlapped Main's run start, and my 10k smoke overlapped the
items-115/116 cert restart (killed within ~2 min, disclosed both). Compose
project-name collision between worktrees discovered by items-115/116, fixed on
main by #213. MEMORY roll-up #2 performed (52→31 KB, entries before 07-22
archived). Lesson reinforced: `docker ps` between another session's bench phases
is NOT a sufficient go-signal — wait for the explicit all-clear.

### 2026-07-23 — Fresh full Docker bench on main `0324dc5` → new MM_BASELINE; item 114 filed

User asked to verify the branch matched main, then run backlog Next-up #1. Branch
`claude/pending-items-bce650` == `origin/main` (`0324dc5`), tree clean. Pre-flight per
LESSONS: no stray bench processes, `docker compose down -v`, unrelated idle containers
(pg-demo on :5433, minio) verified non-conflicting and left running. Full
`scripts/report.sh --docker` (no skip knobs, no stitch): 84m 58s. compare_bench.py arg
order is `<run> <baseline>` (first attempt reversed produced mirror-image deltas — check
the "vs <file>" header line). Auto-compare at end of report.sh used the stale 07-17
`benchmark_*` file (docker/out is gitignored, so a fresh worktree only had old promoted
copies) — always re-compare manually against the intended baseline. Recorded: PROGRESS
entry + index row, performance README pointer, backlog index (Next-up refreshed, 114
registered, next→115), `114_w4_event_rung_tax.md` filed. Docs lints green before push.

### 2026-07-22 (session close) — items 110 + 111 shipped, 112 filed, Z4 status corrected

110: root cause one layer under the filing's analysis — no `LogicalPlan::Query` arm in
`substitute_current_user_in_plan` + eager Expr→QExpr conversion whose fallback rewrote
CurrentUser→Bool(true) (policy weakening hazard). Fix at injection time + fail-closed Null.
111: `is_information_schema` exemption in check_plan_privileges + per-caller ANY-privilege row
filter in `virtual_rows` (now takes user); constraint views included; open/superuser mirror
`is_effective_superuser`. 112: Z4 audit — inheritance transitive & shipped; column grants
scoped into their own parked item with full touch-point map. All suites green each time
(70-72 binaries, crash 54/54). Eight PRs merged this session: #190-#193, #196-#200 (#195
rescued/renumbered). Hygiene rules that stuck: one PR per unit (squash-merge orphan race),
benches get exclusive machine time, `docker compose down -v` before bench reruns, verify main
by ls-tree not PR state.


> **Older session-log entries (before 2026-07-22) live in
> [`docs/history/MEMORY_ARCHIVE_2026-07.md`](docs/history/MEMORY_ARCHIVE_2026-07.md)
> (rolled 2026-07-22 and 2026-07-24).**
