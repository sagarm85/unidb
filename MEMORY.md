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

- **2026-08-02 — Parity track confirmed fully MERGED: PRs #235–#249 (+ docs #250) are all on `main` (verified against `origin/main` `a3cd132`).** The entry below originally said "pending merge" — that went stale the moment the merges landed; corrected inline rather than silently rewritten. The missing consolidated ledger entry was backfilled the same day: see PROGRESS.md "Supabase-parity BaaS layer — items 120–133 + free-parity continuation 135–146".

- **2026-08-01 — Supabase free-parity continuation (items 132–146) SHIPPED; ~~17 PRs #235–#248 + JWT rotation (146) pending merge~~ (all merged — see the 2026-08-02 correction above).**
  Everything on the free/self-hostable roadmap (`docs/backlog/137`) that needs no paid third-party,
  built as verified per-PR merges, all **plan-time/control-plane → crash 54/54 by construction**:
  132 realtime broadcast+presence · 133 GraphQL mutations · 135 server-full fixes · 136 /rest/v1
  embed filter/order · 138 email transport (SMTP+dev-log) + password-reset + magic-link · 139 REST
  count=exact/`Prefer` · 140 realtime channel-authz (role/topic-glob, opt-in `UNIDB_REALTIME_REQUIRE_AUTHZ`,
  audited bypass) · 141 database webhooks (durable consumer, HMAC-signed, retry-then-skip) · 142 auth
  admin API (`/auth/admin/users`, ban, app/user metadata, last-superuser guards) · 143 HIBP
  leaked-password (opt-in, fail-open) + 5 OAuth presets · 144 cron scheduled jobs (`/cron/jobs`,
  `run_as` RLS parity) · 145 `GET/DELETE /auth/dev-inbox` (superuser + dev-transport-only) · 146 JWT
  signing-key rotation (`kid` header + verify-only previous-key grace window + multi-key JWKS). Plus
  **unidb-js SDK completion** (storage/GraphQL/broadcast/presence, 55/55) + npm CI/publish workflows
  pushed to `sagarm85/unidb-js`. Pattern: file `NN_` spec → Sonnet agent → independent verify (build +
  clippy --all-targets + `cargo test --no-run` no-features + crash 54/54 + targeted/regression) →
  reset-author, force-with-lease push, squash-merge. **Held for design/user go-ahead:** GraphQL
  subscriptions (WebSocket-vs-SSE call), the engine-core cluster (stored functions → RPC/triggers/
  upsert — ACID write-path), storage TUS/image-transforms, SAML.
  (The transient per-item "committed to branch" working notes were consolidated here on merge; the
  durable per-item detail lives in each `docs/backlog/NN_*.md` file's SHIPPED status + `PROGRESS`.)

> **Older Current-status entries (2026-07-24 and earlier) were rolled into
> [`docs/history/MEMORY_ARCHIVE_2026-07.md`](docs/history/MEMORY_ARCHIVE_2026-07.md)
> across the 2026-07-22 and 2026-08-01 roll-ups. Grep there for any dated
> entry; nothing was deleted.**

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

Nothing in flight. M0–M5 (original roadmap) and the Supabase-parity BaaS
track (items 120–146, PRs #222–#250) are all merged to `main` — see
PROGRESS.md "Supabase-parity BaaS layer — items 120–133 + free-parity
continuation 135–146". What remains, and its gating:

- **HELD for explicit user/design go-ahead** (do not start unprompted):
  engine-core compute cluster (stored functions → RPC / triggers / upsert
  `ON CONFLICT` — ACID write-path), GraphQL subscriptions (WebSocket-vs-SSE
  call), storage TUS uploads + image transforms, SAML. See
  `docs/backlog/137_supabase_parity_free_roadmap.md` for the full open list
  (also: views/enums, identity linking, anonymous sign-in, SDK breadth).
- **Approved-but-unbuilt:** item 118 (async-HNSW crash reconciliation —
  plan approved 2026-07-31).
- **Standing §6 debt:** the replaced-stack headline column (Table 4.1,
  `MM_REPLACED_STACK=1`) has never been measured, and the BaaS/HTTP layer
  has had no load benchmark of its own — both are the honest gaps in the
  current evidence base.

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

### 2026-08-03 (cont. 2) — item 149 (row triggers) shipped; PROGRESS rolled up (121→32 KB)

149 done the same way (Sonnet agent → orchestrator re-verified: item149
20/20 · item150 20/20 · **crash 58/58** incl. p149a/b audit-atomicity ·
147/148/constraints/server_rest green). Same-txn per-row triggers on
item-147 functions; NEW./OLD. → lexical $n rewrite; no-cascade rule;
fires on upsert's DO UPDATE arm too. §0.6 flags recorded in the PROGRESS
entry: triggered tables lose batch/fast paths + take the exclusive catalog
lock — measure, don't assume. PROGRESS.md crossed 120 KB → rolled 07-20→
07-24 entries verbatim into `docs/history/PROGRESS_ARCHIVE_2026-07.md`
(index rows flipped to archive). PR raised on push (stacked on 150's
PR #257 — merge that first). Compute cluster now fully shipped:
147+148+149+150; auth hooks = the remaining unlock.

### 2026-08-03 (cont.) — item 150 (upsert ON CONFLICT) shipped; LATENT HOT-CHAIN MVCC BUG FOUND & FIXED; 149 next

Item 150 implemented by its Sonnet agent and orchestrator-verified (item150
20/20 · REST 7/7 · **crash 56/56** — 2 new upsert injection points ·
constraints/148/147/RLS/server_rest all green). The headline is NOT the
feature: the spec's required concurrency test exposed a **pre-existing
severe MVCC read-path bug** — `heap::get_visible_cached`/`_with_rid`
followed at most one HOT-chain hop, so after ≥2 sequential HOT updates on a
PK/UNIQUE-indexed row the index candidate under-resolved to "no visible
version" and a duplicate-key INSERT could slip past `enforce_unique` (two
live rows, same key, empirically reproduced). Fixed by a bounded chain walk
(`MAX_HOT_CHAIN_HOPS`), read-path only, regression test added. **Flag: the
open item-16 concurrent-visibility anomaly (3-visible-rows churn repro,
default-OFF toggle blocker) is in the same under/over-resolution family —
RETEST that repro on top of this fix before assuming anything; do not
silently close item 16.** Upsert itself: routing through existing machinery
(probe = enforce_unique pattern; DO UPDATE arm = `apply_single_row_update`
extracted from exec_update), RLS fail-closed both arms, REST
`on_conflict=`/`Prefer: resolution=` wiring (139's exclusion removed).
Next: item 149 (triggers) branch stacks on 150's exec_insert restructuring
(`try_insert_one_row`) — its agent prompt must point at the refactored
shape, not the spec's original exec_insert assumptions.

### 2026-08-03 (later) — item 148 (enums + domains) shipped the same way; 147 merged (PR #253)

Same pattern, second item: the 148 Sonnet agent implemented the locked spec
(sqlparser-0.62 AST route — CreateType/CreateDomain are dialect-unconditional,
no pre-parse hack needed; the one real correctness catch was that the
executor's two-valued `compare` would reject NULL through a bare enum CHECK,
fixed by wrapping every synthesized CHECK in `col IS NULL OR (…)`).
Orchestrator re-verified independently, including post-merge with 147's
implementation after `origin/main` (PR #253 merged mid-run) was merged in:
item148 16/16 · item147 8/8 · constraints 30/30 · RLS 8/8 · crash 54/54.
Two union-merge conflicts (error.rs variants, backlog_index) resolved by
keeping both items' content. Ship records: PROGRESS entries for both items
(147 flipped to merged-PR-#253, 148 added), SUPABASE_PARITY counts now
20/9/4/16 (enums+domains → Partial table; composite types stay deferred),
147 spec + index row → SHIPPED, 137 lines → DONE. PR for 148 raised on push
per the user's standing no-go-prompt instruction (2026-08-03). Engine-core
next phases still open: triggers, upsert `ON CONFLICT`, auth hooks (specs
not yet written — next session's decision with the user).

### 2026-08-03 — item 147 (stored functions v1 + RPC) implemented via Sonnet agent, verified, committed to branch; item 148 queued

User go-ahead (2026-08-02) lifted the compute-cluster HELD; two branches
started (per explicit user instruction, and the user chose Sonnet for
implementation): **147** `feat/147-stored-functions-rpc` — spec written by
orchestrator, implemented by a Sonnet subagent, **all 7 gates re-run
independently by the orchestrator** (clippy/fmt/no-features compile, item147
8/8, item144 9/9, crash 54/54), committed `b5c0156` + main merged in +
SUPABASE_PARITY/PROGRESS/MEMORY updated in the same push. Design's one
security-critical call: RPC is invoker-by-default (`run_as: None` = caller,
NOT admin — deliberately diverges from cron's default; spec documents why).
**148** `feat/148-enums-domains` — spec committed (`1b1ea2b`, written in a
side worktree at the scratchpad so the 147 agent's tree stayed untouched);
implementation queued strictly behind 147's builds per LESSONS.md's
no-concurrent-cargo-builds rule. PR #252 (SUPABASE_PARITY.md tracker)
merged this session; tracker rows for 147/148 updated on the 147 branch in
the same PR as the code, per the tracker's §9 protocol. Studio-side asks
(favicon, timestamp display) were explicitly dropped by the user as
wrong-session; the favicon SVG was delivered in-chat for the studio session
to pick up. Pre-existing named-superuser `WITH CHECK` quirk (item-133
finding) re-surfaced by 147's parity test — still untouched, still tracked.

### 2026-08-02 — Docs-staleness sweep: parity track confirmed merged; PROGRESS ledger backfilled; unidb-js linked

Q&A session (Supabase-service comparison, branch
`claude/supabase-service-comparison-10ssl8`) turned up doc drift; user asked
for a full reference-doc refresh so future sessions don't inherit wrong
assumptions. Verified against `origin/main` (`a3cd132`): **PRs #222–#250 are
ALL merged** — the top Current-status "pending merge" claim was stale. Fixed:
(1) Current-status corrected inline + **In progress** section rewritten (was
still M5-era text) to list the real remaining work with its gating
(HELD-for-sign-off compute cluster / GraphQL subs / TUS / SAML; approved item
118; unmeasured Table 4.1 + no BaaS-layer load bench); (2) **PROGRESS.md got
the missing consolidated ledger entry** for items 120–146 — the per-milestone
PROGRESS habit was skipped during the overnight merge run (process lesson:
flipping a backlog file's Status on merge is NOT the ledger entry); (3)
`137_supabase_parity_free_roadmap.md` per-item "IN PROGRESS — flips on merge"
lines flipped to DONE with PR numbers, held items labeled HELD; (4)
`backlog_index.md`: 137 row refreshed + the "Next up" pointer un-staled
(134 → 137); (5) **README.md + docs/documentation_index.md now link the
`unidb-js` SDK** — it was previously unmentioned in every user-facing doc
(found while answering "should the repo reference unidb-js?"). Also noted
during the session (report-honesty, no doc change yet): MEMORY's 07-28 entry
calls the moat "intact" citing W4/W0 13.56×, but W4/W0 is the internal tax
ladder — the actual moat column (replaced stack, Table 4.1) was skipped in
that report; the queued report-honesty item covers it. No code change.
**Same-day follow-up (after the sweep merged as PR #251; branch restarted
from `main` per the merged-PR rule):** added root **`SUPABASE_PARITY.md`** —
a living four-table feature-parity matrix vs supabase.com/features (done /
partial / done-differently / not-done, per-row scope + item numbers, summary
counts, "Last verified" stamp) — linked from `README.md`,
`docs/documentation_index.md`, and `137`'s header, and registered in
`CLAUDE.md` §9's pre-push checklist so every future parity ship updates it
in the same PR.

> **Older session-log entries (the rest of 2026-07-31 and earlier) were rolled
> into [`docs/history/MEMORY_ARCHIVE_2026-07.md`](docs/history/MEMORY_ARCHIVE_2026-07.md)
> on 2026-08-01. Grep there for any dated entry; nothing was deleted.**
