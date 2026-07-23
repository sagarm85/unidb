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

- **Item 108 — RESOLVED 2026-07-21 (same day): CRUD drift was ENVIRONMENTAL, no unidb regression.**
  Absolutes (§0.6 rule 4): PG's code-identical absolutes moved 2.1–28× between the 07-19/07-21
  runs (VM fsync ~30×, CPU ~2.15× — why the 07-19 run took 229 min); unidb improved on EVERY row
  in absolutes (INSERT 138→4,128 rec/s; filtered 812k→2.72M) and WAL-B/row (INSERT 6,366→584 —
  item 104's signature; HOT 154→88; DELETE sel 39→5). No bisection needed. Shipped: PG-absolute
  environment canary in compare_bench.py (median drift >25% → warning; fires at 173% on this
  pair), decompose.rs ceilings table refreshed to 07-21 values, inline correction of the
  item-104 COUNT claim in PROGRESS.md (real evidence = WAL-B/row + W0, not the COUNT ratio).
  Rule: cross-run ratio deltas are evidence only when the canary is quiet; else judge by
  absolutes + WAL-B/row.

- **Consolidated Docker bench — RUN + RECORDED 2026-07-21** (`docs/performance/report_20260721_035629.md`,
  94m 54s total — bench got 2.4× faster since 07-19 because HNSW insert improvements shrank the ladder;
  promoted as canonical benchmark + standing `MM_BASELINE`). Verdicts: **item 104 VALIDATED**
  (W0 0.23 ms/commit at 100k; COUNT(*) 6.93→**41.25×**); **items 72/73/93+NodeCache VALIDATED**
  (Table 4 at 100k 81.8→13.4 ms/txn, 6.1×); **conc matrix 32/32 PASS**. Two findings filed:
  **item 107** — synchronous HNSW insert breaks W4≈W0 (Δvector +17.6 ms/commit at 100k, W4/W0 96×,
  Table 4 0.01×; M2's locked design = async worker; Step-0 audits item 67 coverage + freshness
  contract); **item 108** — CRUD ratio drift vs 07-19 (SELECT filtered 0.74→0.45×, UPDATE HOT
  1.51→1.06×; classify via absolute rec/s then bisect with item-105 selective runs; also refresh
  the stale ceilings table in decompose.rs). Linux NEAR spot-check still open (mmreport doesn't
  measure NEAR; run perf_item92 in-container).

- **Item 92 Levers 5+7 — SHIPPED 2026-07-21; acceptance revised ≤700 µs → ≤1 ms WITH USER SIGN-OFF same day; pgvector-class tier filed as item 106.**
  10k re-profile: warm NEAR 2,091 µs, 1,257 µs unattributed → root cause: `exec_select_near`
  deep-cloned the ENTIRE per-index cache per query (L0 arena + 10k-entry vec HashMap ≈ 7 MiB +
  10k allocs, plus 10k-entry merge-back walk; O(corpus)/query, ~15 ms at 100k). Lever 5: Arc
  copy-on-write snapshots + storage_ptr merge-skip → **895.5 µs (−57%)**. Lever 6 (fast hasher)
  REJECTED on 3×3 A/B (wash) and reverted. Lever 7: `VecArena` contiguous vector slab (item 93
  pattern) → **~900 µs mean, variance ±120→±2 µs**. Phase timers added (permanent):
  ANN ~605 µs · re-rank ~222 µs · parse/plan ~74 µs. Recall pinned 0.900 throughout.
  ≤700 µs unmet; user signed off Option A same day: acceptance revised to ≤1 ms (achieved
  ~900 µs), item 106 filed for the pgvector-class ≤400 µs tier (graph quality/SQ8/PQ).
  All tests green (30 binaries + 54/54 crash). Docker/Linux NEAR check + W2-rung no-regression
  fold into the pending consolidated bench run.

- **Item 105 — Selective bench runs + baseline carry-forward — SHIPPED 2026-07-21, branch `claude/session-status-check-fae1c3`.**
  Root-caused the ~4 h `report.sh` wall clock (per-phase docker-stats sample counts in
  `report_20260719_234504.md`): Tables 1+2 W0→W4 ladder ≈ 2.5 h (synchronous HNSW/graph
  pre-grows — items 63/65/92 bottleneck), Table 4 @100k ≈ 45 min, rest minutes.
  Three bugs fixed: (1) `MM_TABLES`/`MM_SKIP_TABLE4`/`MM_SKIP_TABLE5` were NOT threaded
  through `docker-compose.yml` — Docker-mode selective profiles silently ran the full bench;
  (2) allowlist only honored by Tables 4/5 — 1/2/3/3.1 always ran; (3) `compare_bench.py`
  Table-4 rows clobbered W4/W0 entries (same integer+ratio row shape).
  Shipped: `MM_SKIP_LADDER=1` (Tables 1+2 gate, `_Skipped:` markers), knobs threaded through
  Docker, `scripts/stitch_baseline.py` + `MM_BASELINE=<report.md>` carry-forward with
  provenance stamp ("Carried forward — NOT re-measured in this run", source/commit/date),
  section-aware `compare_bench.py` excluding stitched tables. CRUD-item run ~4 h → ~30–45 min.
  Guardrail: carry-forward invalid for shared-layer changes (WAL/commit/pool/heap/format);
  full baseline still mandatory per major release. Smoke-verified (denylist + allowlist +
  stitch on real reports); clippy `--bench decompose` clean (4 pre-existing lints fixed),
  fmt clean. Docs: backlog `105_…`, PROGRESS.md, scripts_guide.md, report.sh header.

- **Item 104 — Catalog sync dedup — SHIPPED 2026-07-20, PR [#180](https://github.com/sagarm85/unidb/pull/180) open.**
  Removed `wal.sync_up_to(catalog_lsn)` AND `catalog.persist_only()` from `Engine::commit`.
  (Just removing the fsync while keeping `persist_only()` caused a replication regression:
  `persist_only()` flips `catalog_root` in the control file, but without the matching fsync the
  catalog WAL records weren't in the shipped replication stream → `SlotOutOfRange` on replica.)
  The correct fix: row_count is updated in-memory on every commit, persisted to disk only at
  checkpoint. Added `ROW_COUNT_UNKNOWN = i64::MIN` sentinel; `Catalog::load` resets all
  `row_count` to UNKNOWN on open. COUNT(*) fast path falls back to `count_visible` heap scan
  when UNKNOWN; caches result back if catalog handle is Exclusive. Delta-apply guards UNKNOWN base.
  New crash test `p104_catalog_sync_dedup_crash_recovery_count_exact` (4 phases).
  54/54 crash tests, 463/463 lib tests. Clippy + fmt clean.
  **Expected: ≥ 1.3× INSERT throughput (eliminates per-commit serialized fsync).**

- **Item 102-B — Covering index INCLUDE columns — SHIPPED 2026-07-20, PR [#177](https://github.com/sagarm85/unidb/pull/177) MERGED.**
  `CREATE INDEX ON t (col) INCLUDE (c1, c2, …)` — stores INCLUDE col values in B-tree leaf.
  Leaf wire format: `key_bytes | include_len:u32-LE | include_bytes | RowId(6B)`.
  FORMAT_VERSION 11→12. `ColumnDef.include_cols: Vec<String>` persisted via `set_column_include_cols`.
  Optimizer: `index_only = projection ⊆ {key} ∪ include_cols`. Covering fast path in `try_exec_select_btree`:
  `search_with_keys_and_include` → decode include bytes → project by name. `IDX_INCLUDE_ROWS` counter.
  HOT eligibility gate: SET on INCLUDE col disables HOT (else stale include bytes in leaf).
  WAL type-15 extended with `include_len(4B)|include_bytes`; `redo_index_insert_with_include` in recovery.rs.
  Bulk build via `insert_many_with_include`. UPDATE covering maintenance via `IndexColBatch.include_entries`.
  Parser: `INCLUDE (cols)` clause, `None => IndexKind::BTree` default.
  Tests: 10 new in `tests/item102b_covering_index.rs`; all 447 unit + 53 crash = 0 failures.
  Clippy + fmt clean.

- **Item 93 — HNSW L0 arena layout — SHIPPED 2026-07-20, PR #175 MERGED.**
  Replaced `HashMap<i64, Vec<RowId>>` in `HnswL0Cache` with flat `L0Arena` (two `Vec`s:
  `arena_data: Vec<i64>` + `arena_offsets: Vec<u32>`). Hot-path `for_l0_nbrs` iterates
  the arena slice in-place via callback — zero heap allocation per hop. Stack buffer
  `[RowId; HNSW_M_MAX0]` (32 slots) collects neighbours with zero `Vec<RowId>` alloc.
  Tombstone/compact design removed (prefix-sum offset array cannot support per-slot zeroing).
  Measured (debug): recall@10=1.000 (≥0.90), disk_fetches=0, L0 hits=3000. Docker bench pending.


> **Older Current-status entries (before 2026-07-20) were rolled into
> [`docs/history/MEMORY_ARCHIVE_2026-07.md`](docs/history/MEMORY_ARCHIVE_2026-07.md)
> on 2026-07-22. Grep there for any dated entry; nothing was deleted.**

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

### 2026-07-21 (same session) — Item 108 resolved same-day: drift = environment

Absolutes-first comparison closed it in one step, no bisection: PG code-identical absolutes
moved 2.1–28× across the two runs; unidb improved everywhere (absolutes + WAL-B/row). Shipped
compare_bench.py env canary (>25% PG-absolute median drift → warning), refreshed the stale
decompose.rs ceilings table (now 07-21 values + absolutes-first protocol note), corrected the
item-104 COUNT-baseline claim in PROGRESS.md inline. Item 107 (async HNSW) is now the sole
open finding from the bench.

### 2026-07-21 (same session, after item 92) — Consolidated Docker bench + items 107/108 filed

Full run on main+92 (`b6d6e5f`): 94m 54s (vs ~230 min on 07-19 — ladder now cheap since HNSW
insert improvements; validates item 105's timing analysis). Debt verdicts: item 104 ✓ (W0
0.23 ms at 100k, COUNT 41.25×), items 72/73/93+gate ✓ (Table 4 100k 81.8→13.4 ms/txn), conc
32/32 PASS. Findings: W4/W0 blown to 19–96× — synchronous HNSW insert (Δvector +6.6→+17.6
ms/commit); old ≈1.5× baseline was IVF-era; fix = M2's prescribed async worker → **item 107**.
CRUD drift vs 07-19 (filtered 0.74→0.45×, HOT 1.51→1.06×) → **item 108** (absolute-first
classification, then item-105 selective bisect; refresh stale decompose.rs ceilings table).
Report promoted (benchmark_20260721_133227.md) + copied to docs/performance as MM_BASELINE.

### 2026-07-21 (later same session) — Item 92 Levers 5–7: NEAR warm 10k 2,091 → ~900 µs

**Goal:** user said "start item 92, then plan Docker bench validation debt." Discovered item 92
was further along than the index suggested (levers 1–3 already merged, PR #154); remaining =
the two unchecked acceptance boxes. Native 10k probe showed levers 1–3 did NOT scale: 2,091 µs
warm (vs 921 µs at 2k), 1,257 µs unattributed.

**Lever 5 (shipped):** root cause of unattributed block = `exec_select_near` deep-cloning the
entire per-index cache per query (L0 arena + vec HashMap ≈ 7 MiB + 10k allocations at 10k, plus
O(n) merge-back walk). Fix: `Arc` copy-on-write storage in `HnswVecCache`/`HnswL0Cache`
(`Arc::make_mut` on mutation), `storage_ptr()` compare to skip merge-back when nothing inserted,
ptr-eq/empty-adopt fast paths in `merge_from`. Measured: 2,091 → 895.5 µs (−57%), cold
2,331 → 1,499 µs, counters/recall identical.

**Lever 6 (rejected honestly):** hand-rolled FxHash-style hasher for the 4 hot structures;
3×3 A/B showed FastHash ~996 vs SipHash ~992 µs — wash. Hashing is not the bottleneck (memory
pointer-chase is). Fully reverted; recorded in backlog 92 as do-not-reattempt-without-evidence.

**Phase attribution (permanent):** `Q_ANN_NANOS`/`Q_RERANK_NANOS` atomics in `exec_select_near`,
printed by perf_item92. Warm split: ANN ~605 µs (66%) · re-rank+project ~222 µs (25%) ·
parse/plan/snapshot ~74 µs. Stale heuristic attribution print (claimed Vec clones + "SIMD
could 4-8×" after SIMD had shipped) replaced with measured split.

**Lever 7 (shipped):** `VecArena` — one flat `Vec<f32>` slab + key→slot map replacing 10k
scattered 512 B Vec allocations (item 93's L0Arena pattern; drop-in behind Lever 5 accessors).
Measured: 897.9/899.7/902.1 µs — mean ~900 µs (~9% under Lever-5-alone mean ~990), variance
±120 → ±2 µs. Locality hypothesis mostly didn't pay (ANN still ~605 µs — 5 MiB random-access
working set); honest wins: determinism, allocator pressure, single-memcpy COW.

**Target status:** ≤700 µs not met (~900 µs native macOS, recall pinned at exactly 0.900);
remaining micro-levers floor ≈700–750 µs. **User signed off Option A same day**: acceptance
revised to ≤1 ms (achieved), item 106 filed for the pgvector-class ≤400 µs tier (Step-0 =
recall-vs-ef curve, then graph-quality heuristic / SQ8 slab / decode-pushdown).
Verification: 30 test binaries green, crash 54/54, clippy/fmt clean.
Flagged as spawn-task chips: item102 IDX_ONLY_ROWS test race (observed flaking once),
pre-existing clippy lints in 4 test binaries (--all-targets not in the gate).
Committed + PR'd after sign-off; consolidated Docker bench launched same session.

### 2026-07-21 — Item 105: Selective bench runs + baseline carry-forward (bench tooling)

**Goal:** User asked why `report.sh` takes 3–4 h per validation and proposed reusing prior
bench tables for unaffected areas. Root-caused the time, fixed the knob plumbing, shipped
carry-forward stitching.

**Root cause of the 4 h:** per-phase docker-stats sample counts (`n` ≈ seconds) in
`report_20260719_234504.md` (230 min): Tables 1+2 W0→W4 ladder ≈ 2.5 h (W2–W4 pre-grows
build HNSW + graph indexes synchronously — the item 63/65/92 incremental-HNSW bottleneck);
`t4_unidb_100000` n=2595 ≈ 43 min; everything else minutes. ~85 % of bench wall clock IS
the slow HNSW insert path — fixing item 92 shrinks the report for free.

**Bugs found:** (1) `MM_TABLES`/`MM_SKIP_TABLE4`/`MM_SKIP_TABLE5` never passed through
`docker-compose.yml` → Docker-mode per-item profiles silently ran the full ~4 h bench
(this is why the user's run took 4 h despite documented ~1.5 h profiles). (2) Allowlist
only honored by Tables 4/5; 1/2/3/3.1 always ran. (3) `compare_bench.py`: Table 4 rows
(integer first col + `×` last col) clobbered Table 1's W4/W0 entries.

**Shipped:** `MM_SKIP_LADDER=1` + full `MM_TABLES` gating in `benches/decompose.rs`
(`_Skipped:` markers under skipped `## Table N` headings; 1+2 one unit, 3.1 gated with 3);
knobs threaded through `docker_report.sh` + compose; new `scripts/stitch_baseline.py` +
`MM_BASELINE=<report.md>` hook in `report.sh` (host-side post-processing, both modes) —
skipped tables carried forward with provenance stamp ("Carried forward — NOT re-measured
in this run", source file/commit/date; holes never copied; chained stamps preserved +
warned); section-aware `compare_bench.py` excludes stitched tables. Fixed 4 pre-existing
`needless_range_loop` clippy lints in the bench (only visible with `--bench decompose`).

**Verification:** debug-bench smoke (denylist → 4 markers, Tables 3/3.1 measured;
`MM_TABLES=3` → only 3/3.1); stitch tested against real reports; parser confirmed
excluding carried tables. clippy/fmt/bash -n/compose config all clean. New CRUD-item
profile: `MM_SKIP_LADDER=1 MM_SKIP_TABLE4=1 MM_SKIP_TABLE5=1 MM_BASELINE=… scripts/report.sh`
→ ~30–45 min. Guardrail: carry-forward invalid for shared-layer changes; full baseline per
major release. Docs: backlog `105_bench_selective_carry_forward.md` (+index, next→106),
PROGRESS.md entry, scripts_guide.md, report.sh/multi_model_report.sh headers.

### 2026-07-20 — Item 104: Catalog sync dedup (remove double-fsync per INSERT)

**Goal:** Remove `wal.sync_up_to(catalog_lsn)` from `Engine::commit` — the second fsync added
by item 97 that ran outside the group-commit window, halving INSERT throughput under concurrent load.

**Changes shipped:**

1. `src/catalog.rs` — Added `pub const ROW_COUNT_UNKNOWN: i64 = i64::MIN`. `Catalog::load`
   calls new `reset_row_counts_unknown()` helper after parsing, resetting all table
   `row_count` to `ROW_COUNT_UNKNOWN` so COUNT(*) falls back to heap scan after crash.

2. `src/lib.rs` — Removed `wal.sync_up_to(catalog_lsn)` AND `catalog.persist_only()` from
   `Engine::commit`. Retaining `persist_only()` caused replication regression: it flips
   `catalog_root` in control file per commit, but without matching fsync the catalog WAL records
   weren't in the shipped stream → replica gets `SlotOutOfRange`. Fix: in-memory only in commit;
   checkpoint persists. Added UNKNOWN guard in delta application. Import `ROW_COUNT_UNKNOWN`.

3. `src/sql/query_exec.rs` — Extended item 97 COUNT(*) fast path: when `row_count == UNKNOWN`,
   falls back to `count_visible` heap scan; caches result via `exclusive()` if handle permits.
   Import `ROW_COUNT_UNKNOWN`.

4. `tests/crash/main.rs` — Added `p104_catalog_sync_dedup_crash_recovery_count_exact` (4 phases:
   insert 100 rows + crash, COUNT=100, COUNT=100 again, insert 50 more + COUNT=150). All three
   COUNT checks use heap scan (UNKNOWN sentinel). 54/54 crash tests PASS.

5. `docs/backlog/104_catalog_sync_dedup.md` — created (SHIPPED status).

6. `docs/backlog/backlog_index.md` — item 104 registered (SHIPPED); "Next new file → 105_…".

7. `PROGRESS.md` — Item 104 entry added.

**Durability contract change:** `row_count` on disk is now checkpoint-granularity (was commit-granularity).
COUNT(*) is always exact in memory and always exact after crash (via heap scan recalibration).

**Tests:** 54/54 crash PASS. 463/463 lib unit tests PASS (replication tests apply_is_idempotent +
base_plus_incremental_then_promote now PASS — were failing with just sync_up_to removed). Clippy + fmt clean.

**Performance:** Docker bench pending. Expected ≥ 1.3× INSERT throughput under 32 concurrent writers.

---

### 2026-07-20 — Item 102-B: Covering index INCLUDE columns

**Goal:** `CREATE INDEX ON t (col) INCLUDE (c1, c2, …)` — store include column values in
B-tree leaf entries so `SELECT col, c1 FROM t WHERE col = val` is served from the leaf without
calling `deform_row` on the heap tuple. Heap.get() still called for MVCC visibility.

**Changes shipped:**

1. `src/format.rs` — FORMAT_VERSION 11→12.

2. `src/catalog.rs` — `ColumnDef.include_cols: Vec<String>` (`#[serde(default)]`);
   `set_column_include_cols` method persists include_cols after index build.

3. `src/btree_index.rs` — `Node::Leaf { include_payloads: Vec<Vec<u8>> }` parallel vec;
   `node_is_insert_safe` takes `include_payload_len: usize` to correctly account for payload overhead;
   `insert_in_txn_with_include` full crabbing descent with include payload;
   `insert_with_include`, `insert_many_with_include` (bulk covering build, single mini-txn);
   `search_with_keys_and_include` → `Vec<(OrderedValue, Vec<u8>, RowId)>`;
   `redo_index_insert_with_include` (WAL redo with include bytes);
   `insert` / `insert_in_txn` / `redo_index_insert` all delegate to `_with_include(..., &[])`.

4. `src/wal.rs` — `log_index_insert_with_include`: type-15 record extended with
   `include_len(4B) | include_bytes`; `log_index_insert` delegates to it with `&[]`.

5. `src/recovery.rs` — WAL_INDEX_INSERT redo block parses `include_len + include_bytes`
   suffix (zero-length = non-covering, backward-compatible); calls `redo_index_insert_with_include`.

6. `src/sql/parser.rs` — `INCLUDE (cols)` clause; `None => IndexKind::BTree` default
   (fixes `CREATE INDEX ON t (col) INCLUDE (…)` without USING).
   `src/sql/logical.rs` — `CreateIndex.include_cols: Vec<String>`.

7. `src/sql/executor.rs` — `IDX_INCLUDE_ROWS: AtomicU64` counter; optimizer extends
   `index_only` check: `projection ⊆ {key_col} ∪ include_cols`; covering fast path in
   `try_exec_select_btree` (`search_with_keys_and_include` → `decode_row` → project);
   `IndexColBatch.{include_cols, include_entries}` for UPDATE covering maintenance;
   `set_touches_indexed_col` also fires for SET on INCLUDE col (HOT gate);
   `apply_durable_index_writes` encodes include payload from row values;
   `exec_create_index` bulk-collects `include_pairs` → `insert_many_with_include`.

8. `tests/item102b_covering_index.rs` (new) — 10 tests: parse_and_build,
   idx_include_rows_counter, star_projection_heap, non_include_col_heap,
   update_include_col, delete_row, multi_include_cols, range_predicate,
   reopen_survives, perf_10k_covering.

9. `tests/item102_index_only_scan.rs` — removed `before==after` counter assertion
   in `star_projection_uses_heap` (global counter + parallel test runs = flaky);
   verification now via column count (SELECT * returns all cols → proves heap access).

10. Docfixes: `docs/backlog/102_index_only_scan.md` Status → SHIPPED;
    `docs/backlog/backlog_index.md` item 102 row updated, Next-up section revised.
    `PROGRESS.md` — "Item 102-B" entry filed.

**Bugs fixed en route:**
- `SqlUnsupported("unsupported index type: None")` — `CREATE INDEX ... INCLUDE` without USING.
- `index out of bounds: len is 1 but index is 1` — `include_payloads` vec not grown before
  `insert_pos` assignment when vec was empty.
- `root split without meta latch held` — `node_is_insert_safe` underestimated leaf entry size
  for covering inserts (fixed by adding `include_payload_len` param).
- HOT update returning Null — SET on INCLUDE col took HOT path (skips B-tree maintenance).
- Reopen returning Null — WAL type-15 record didn't carry include bytes; recovery restored empty.
- Test parallelism flakiness — global `IDX_INCLUDE_ROWS`/`IDX_ONLY_ROWS` counter can increment
  from concurrent tests; `before==after` assertions changed to correctness checks (column counts,
  row values) for "must NOT use covering path" cases; "must use covering path" cases still use
  `after > before` which is safe.

**Test results:** 10/10 new tests PASS. 447 unit + 53 crash = 0 failures. Clippy + fmt clean.

---


> **Older session-log entries (before 2026-07-20) live in
> [`docs/history/MEMORY_ARCHIVE_2026-07.md`](docs/history/MEMORY_ARCHIVE_2026-07.md).**
