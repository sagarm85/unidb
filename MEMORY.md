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

- **2026-08-01 (session close) — items 132 (realtime broadcast/presence) + 133 (GraphQL mutations) SHIPPED; all remaining Supabase-parity work consolidated in backlog item 134 for a fresh session.**
  Two follow-ups to the 120–131 core shipped + merged this session: **133** (PR #235) — a GraphQL
  `Mutation` root (`insert_/update_/delete_<t>`) routed through the same enforced
  `execute_sql_params_as_principal` path as `/rest/v1`/`/sql` (RLS + `WITH CHECK` + column grants
  inherited, parity-tested, requested-projection-only); **132** (PR #236) — in-memory Broadcast +
  Presence over four JWT-gated SSE/POST routes, zero engine/WAL/heap/catalog touch. Both verified
  independently: **crash 54/54**, clippy `--all-targets`, plain `cargo test --no-run`, targeted +
  regression suites all green. **NEXT SESSION START HERE:** everything still open toward Supabase
  parity is in [`docs/backlog/134_supabase_parity_followups.md`](docs/backlog/134_supabase_parity_followups.md)
  — (A) quick correctness follow-ups (named-superuser `WITH CHECK` INSERT quirk, GraphQL bulk
  insert, presence `track` orphan gap), (B) realtime channel-authz + GraphQL subscriptions,
  (C) email transport → magic-link/reset/confirm, more OAuth, storage transforms, DB webhooks,
  scheduled jobs, SDK breadth, (D) cross-repo: unidb-studio panels + unidb-js SDK. Out of scope:
  I6 per-project, I7 edge functions, distributed.

- **2026-08-01 — Supabase-parity BaaS layer COMPLETE (items 120–131) — 12 PRs merged (#222–#233); unidb-js SDK v0.1.0 shipped.**
  A full Backend-as-a-Service layer now sits on the engine, built entirely at plan-time /
  control-plane (no WAL/MVCC/heap/on-disk-format change) so **ACID + perf are intact by
  construction — crash harness 54/54 on every merge.** Shipped:
  - **Auth core (121, A):** argon2id password login/signup (`UNIDB_ALLOW_SIGNUP`), refresh-token
    sessions (256-bit opaque, only SHA-256 hash persisted, rotate-on-refresh, logout revoke),
    production JWT issuer (`UNIDB_JWT_SIGNING_KEY`), asymmetric verify RS256/ES256 +
    `GET /.well-known/jwks.json`. TOTP MFA (D4/127, RFC 6238/4226) + OAuth 2.0 Google/GitHub
    (D1/128, Auth-Code + PKCE).
  - **RLS ↔ token (122, B):** `auth.uid()`, `auth.jwt() ->> 'claim'` in policies (substituted at
    injection time, fail-closed); built-in `anon`/`authenticated`/`service_role` roles;
    `CREATE POLICY … FOR <op> TO <role>`; column-level grants (B5/112, error-not-mask, plan-time
    enforced, policy-column exemption).
  - **Auto API (123, C):** PostgREST-style `/rest/v1/<table>` CRUD+filters (C1, injection-safe
    `$n` binds through the existing RLS/grant path), embedded FK expansion (C2,
    `select=id,customer(name)`), `GET /rest/v1/` OpenAPI (C3), and a schema-derived GraphQL
    endpoint with FK + graph-edge + vector-`near` fields (C4/130, per-field grant correctness),
    now read+write via a `Mutation` root (`insert_/update_/delete_<t>`, item 133, same
    enforced-SQL-path/RETURNING-projection-grant correctness).
  - **Realtime (E1):** per-subscriber RLS-filtered SSE on `/events/subscribe` (service_role
    bypass audited, DELETE filters on the before-image, fail-closed).
  - **Storage (F1/125):** per-object authorization on `/storage/*` (public/private buckets,
    owner/superuser/service_role rules, presign gated, fail-closed).
  - **Ops/hardening:** auth rate-limiting (I1, `429 RATE_LIMITED` + `Retry-After`), encrypt-at-
    rest secrets vault (I3/129, AES-256-GCM, key from `UNIDB_MASTER_KEY`), CAPTCHA/Turnstile on
    auth endpoints (I2/131), forward-only SQL schema migrations + `unidb-migrate` bin with
    checksum drift detection (I4/126), studio-unblocker DDL (124: `ALTER USER … PASSWORD`,
    session list/revoke, policy `target_roles`, signup-binary fix).
  - **unidb-js SDK v0.1.0** (`sagarm85/unidb-js`, separate repo): supabase-js-shaped
    `createClient()` with auth / data (`/rest/v1`) / realtime (SSE); `tsc` strict, 30/30 unit
    tests; ships its own `CLAUDE.md`/`MEMORY.md` for a future session.
  Held per user decision: email/magic-link (D2/D5), SMS (D3). Parked: I5, I7, I6 (per-project
  control plane). Needs the user at deploy time: provider secrets (Google/GitHub/Turnstile/SMTP);
  drive the studio session (build G4 + wire live G1/G2); npm-publish the SDK.

- **2026-07-31 — Architecture-review session: items 117 + report-HTML renderer shipped; item 118 (async-HNSW crash hole) planned next.**
  Fresh senior-architect review of `report_20260728_102745.md` + the async-HNSW/CRUD/scan
  code surfaced a ranked findings list. Two units shipped to main:
  - **Item 117 (PR #218, `c134889`) — HOT UPDATE on PK/UNIQUE tables when the key is unchanged.**
    Root cause: `hot_eligible` gated on `!has_unique`, so the mere existence of a PK/UNIQUE
    index forced every PK'd-table UPDATE onto the slow per-row loop + a redundant per-row PK
    B-tree re-check. Fix mirrors item-53's FK gate: `has_unique_in_set` (HOT/enforce_unique/
    phantom-lock only when a unique/PK col is actually in SET). Safety verified — unchanged
    unique entry resolves via the same `get_visible` HOT-chain walk as secondary indexes.
    **Docker Table-5 cert `report_20260730_124355.md`: UPDATE bulk 0.19×→3.04×, unidb absolute
    138,840→1,130,955 rec/s (+8.1×)** (item-108 caveat: PG absolute drifted, so ~1.5× vs
    baseline PG — still a win, in the Table-3 HOT band). Correctness intact. `can_batch_non_hot`
    left conservative (`!has_unique`) as a follow-up.
  - **Report HTML renderer (PR #217, `71e74b1`).** `scripts/render_report.py` — content-agnostic
    Markdown→styled-HTML; `report.sh` auto-drops a `report_<ts>.html` sibling (git-ignored,
    derived; `.md` stays authoritative). Non-fatal.
  - **Next: item 118 — async-HNSW crash-durability hole (plan approved).** On the served/
    `open_arc` path a committed vector row whose HNSW insert is still in the in-memory
    `sync_channel(4096)` at crash time is lost from the index forever (WAL redo restores only
    what the worker durably applied; no reconciliation). Approved approach: **background,
    crash-gated reconciliation** — an `hnsw_dirty` clean-shutdown flag keeps clean reopens O(1);
    after an unclean shutdown a background thread diffs each index's `node_index` (via
    `validate()`) against the committed heap and re-enqueues only the missing tail through the
    worker. Add `DiskHnswIndex::contains(rid)` (insert is NOT idempotent) + a real crash-
    convergence test. Also folds in the worker's swallowed-insert-error leak.

  Other review findings still queued: report-honesty edits (label the O(1) COUNT(*) row; add
  `W_total`/replaced-stack columns — the moat is unmeasured in the baseline), scan-at-scale
  ceiling (~17M rec/s = per-row TEXT `String` alloc + serial result concat), UPDATE decode-
  pushdown (`cols/row`=8 on HOT UPDATE), and HNSW parallelism (single worker; 16.69 ms drain at
  100k = the Table-4 moat collapse to 0.01×).

- **2026-07-28 — Fresh FULL Docker bench on current main `7c064f1` → new authoritative baseline.**
  `docs/performance/report_20260728_102745.md` (all tables, 83m 38s, RSS 521 MiB, environment
  canary quiet — trustworthy). First full-report capture of items 115/116: **SELECT filtered
  0.58→0.77×** (the #210 one-shot warm-path fix landing in the full CRUD table exactly as its
  cert predicted). Other CRUD vs PG: INSERT per-row 0.57×, UPDATE HOT 1.19×, UPDATE non-HOT
  0.64×, GROUP BY 1.01×, COUNT(*) 49.6×, DELETE selected 1.91×, DELETE all 4.12×. Unified-commit
  moat intact: W4/W0 13.56× at 100k; Table 4 four-model atomic txn vs replaced stack's four
  round-trips. **NOT in this report:** item 106 Unit 3 NEAR win (gate 630→482 µs) — report.sh
  never measures NEAR (standing Linux NEAR-spot-check gap); certified only by native
  `perf_item106`. Promoted as authoritative in `docs/performance/README.md`; supersedes the
  07-23 `0324dc5` baseline.

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

### 2026-08-01 — item 135: `unidb-server-full` wiring fixes (memory storage + ConnectInfo), studio-reported

The studio session, doing live verification, reported two real binary-specific bugs in
`unidb-server-full` (the plain `unidb-server` was already correct). Verified both in source,
fixed both (item 135, PR #238): (1) `try_init_storage` unconditionally built `S3ObjectStore`,
so `STORAGE_BACKEND=memory` demanded S3 creds and never activated → now selects the store by
`cfg.backend` (`Memory` → `MemoryObjectStore`); (2) served with `axum::serve(listener, router)`
instead of `into_make_service_with_connect_info::<SocketAddr>()`, so the item-121 rate-limiter's
`ConnectInfo` extractor 500'd every `POST /auth/{login,signup,refresh}` → now wired like the
plain binary. Also fixed a pre-existing `clippy::manual_ignore_case_cmp` in the `UNIDB_DEV_LOGIN`
parse — **caught because the main-crate `clippy --all-features --all-targets` gate does NOT cover
the separate workspace-member binaries** (follow-up filed to add them to the gate). **Empirically
proven on the live binary** (no committed subprocess harness — not the repo's pattern): booted with
`STORAGE_BACKEND=memory` → log `"storage service ready","backend":"memory"`; `POST /auth/login`
returned 422 (Json body validation), not 500 (past the `ConnectInfo` extractor). Crash 54/54.
**Lesson:** workspace-member binaries (`unidb-server-full`, etc.) escape the main clippy gate —
run `clippy -p <bin>` on them; binary-only wiring (serve/connect-info, env-var reads) isn't
exercised by the library-level `TestServer` and needs a live/binary check.

### 2026-08-01 — item 133 (GraphQL mutations) implemented, committed to branch

Added a `Mutation` root to the C4 GraphQL schema (`src/server/graphql.rs`):
`insert_<t>(values: JSON!): <T>` / `update_<t>(<filter args>, set: JSON!):
[<T>!]` / `delete_<t>(<filter args>): [<T>!]` per eligible table (same
eligibility filter as the query side; a schema with zero eligible tables
stays query-only — an empty GraphQL `Object` type is invalid, so `Mutation`
is only registered when there's at least one field for it). Every resolver
builds one `INSERT`/`UPDATE`/`DELETE ... RETURNING <requested sub-fields>`
statement and runs it through the exact same `run_stmt`/`run_stmts` ->
`authorize_sql_as_principal` + `execute_sql_params_as_principal` path the
query side, `/rest/v1`, and `/sql` already share — no new write path, no
engine change. `RETURNING`'s column list turned out to already be
authorized exactly like a `SELECT` projection (`Engine::check_returning` in
`lib.rs`, pre-existing item-19 machinery) — the RETURNING-parity property
came for free, not something this item had to build. Widened three more
`rest_resource.rs` helpers to `pub(super)` for reuse: `build_assignments`,
`extract_single_in`/`InFilter`, `run_stmts`. New `tests/
item133_graphql_mutations.rs` (7 tests): insert/update/delete end-to-end
returning the requested projection, `WITH CHECK`/RLS rejection parity with
`/sql` (insert + update), and column-grant (RETURNING projection) denial
parity with `/sql`, including a same-statement `/sql` comparison in each
parity test. **Trap found while writing the WITH CHECK test:** a named
`SUPERUSER` principal (e.g. `root` over the HTTP server) is *not* exempted
from a table's own RLS `WITH CHECK` policy by the per-row INSERT check in
`sql/executor.rs::exec_insert` — that check only bypasses for the embedded
(`current_user: None`) caller or an explicit `service_role` claim, not for
`is_effective_superuser` generally (unlike the plan-level `apply_rls` skip,
which does cover named superusers). Not a bug introduced by this item and
out of scope to fix here — noted as a possible pre-existing RLS-superuser
inconsistency worth a future look; the test was adjusted to have the
policy's own owner insert the seed row instead of `root`. All 7 verification
gates green (crash 54/54, server_graphql/server_rest/server_authz all still
pass unchanged). Docs updated: `docs/REST_API.md` C4 section (mutations
documented in full), `README.md` GraphQL bullet, `docs/backlog/
130_graphql_api.md` deferred-note + non-goals pointer to 133. Committed to
the branch (not merged/PR'd — orchestrator flips `133_graphql_mutations.md`
Status to SHIPPED on merge).

### 2026-08-01 — Supabase-parity build drained: 12 PRs merged (#222–#233), autonomous overnight

User authorized autonomous overnight completion + auto-merge of verified PRs, with a
per-merge status report. Drove the entire approved queue to `main`, each PR self-verified
before merge (crash 54/54 + targeted suites + `cargo test --no-run` no-features +
`clippy --all-targets` + fmt): **#222** auth loop (121 A1–A4 + 122 B1–B4), **#223** (121
A5/A6 + I1 + 112/B5 + 123/C1), **#224** E1 realtime RLS, **#225** studio-unblocker (124),
**#226** F1 storage authz (125, salvaged from a stalled agent), **#227** C2 FK-embed (123),
**#228** I4 migrations (126) + a plain-`cargo test` fix, **#229** D4 MFA (127), **#230** D1
OAuth (128), **#231** I3 vault (129), **#232** C4 GraphQL (130), **#233** I2 CAPTCHA (131).
All work is plan-time / control-plane only → ACID + perf untouched by construction (the
crash harness stayed 54/54 through every merge). Separately authored + pushed **unidb-js**
SDK v0.1.0 to its own repo (TS/ESM; auth/data/realtime; 30/30 unit tests) with its own
`CLAUDE.md`/`MEMORY.md`. **Process lessons:** strictly sequential engine builds (usable disk
~38 GiB can't hold two concurrent ~30 GiB `--all-features` Rust builds — `cargo clean`
between every from-clean gate); every new server test needs `#![cfg(feature = "server")]`
or it breaks plain `cargo test`; check a delegated agent's output-file mtime and take over if
it is idle >30 min with no live process (the F1 salvage). Held: email/magic-link (D2/D5), SMS
(D3). Parked: I5/I7/I6. See the 2026-08-01 Current-status entry for the full feature map.

### 2026-07-31 — F1 storage per-object authz (item 125) — salvaged from a stalled agent, verified

The F1 implementing agent went **idle ~5h** (output frozen, no cargo/monitor process, no
completion notification — it hung on its background-test wait). It had left 18 files of complete,
uncommitted work. I took over: dropped its stray `disk_check.txt`, `cargo clean`, and verified the
tree myself — it compiled and passed. **Item 125 / F1** (`8840cdb`): per-object storage authz.
Step-0 caught that bucket public/private never actually shipped and NO caller identity reached
`/storage/*` (every bucket was public to any authed caller). Built: principal threaded into the
storage service; object metadata gains owner (serde-default) + bucket `is_public`; reads gated
(public open, private→owner, superuser/service_role bypass audited, list filtered, presign gated
on the read rule); writes/deletes owner-or-bypass; fail closed. Reuses existing auth machinery.
Verified: crash 54/54, storage_authz_f1 5/5, unidb-storage crate green, clippy/fmt clean. Richer
storage policy-DDL = documented follow-up. **Lesson:** an agent that "waits for a background test
monitor" can hang indefinitely and never notify — check agent output-file mtime vs now; if idle
>~30min with no process, take over and verify the tree directly rather than waiting.

### 2026-07-31 — autonomous overnight: E1 merged (#224) + studio-unblocker batch shipped

Overnight autonomous run (user asleep, auto-merge of verified PRs). **E1** (PR #224, `5b07420`):
per-subscriber RLS on SSE — tenants get only their own events (delivery-side, reuses
`predicate_matches`, service_role audited, fail-closed; crash 54/54, e1 4/4). **Studio-unblocker
batch** (`4008249`, item 124) — 4 gaps the parallel studio session flagged: (1) `unidb-server-full`
now reads `UNIDB_ALLOW_SIGNUP` (was plain-binary-only — real A3 bug); (2) `unidb_catalog.policies`
exposes `target_roles` (role-scoped policy display); (3) `ALTER USER <name> PASSWORD '<pw>'` DDL
(superuser-gated, argon2id, password redacted from Debug/audit) — password reset over /sql;
(4) session listing + revoke-by-id via `unidb_catalog.sessions` (opaque `session_id` independent
of the token hash; view never exposes token/hash; per-caller filtered like item 111) +
revoke-a-specific-session. Verified from clean: both binaries build, crash 54/54,
studio_g1_g2_fixes 7/7, authz/clippy/fmt/lint clean. Filed backlog `124_…`. Disk hit 100% during
the agent build — `cargo clean` (frees ~30G) between every from-clean gate is mandatory now.
**Merged tonight:** #222 auth loop, #223 A5/A6+I1+B5+C1, #224 E1. **Queue:** studio-unblocker PR
(this) → F1 storage per-object RLS → C2 /rest/v1 FK-embed → maybe I4. Deferred-for-user: D
OAuth/email/MFA, H SDK, C4 GraphQL, I2/I3/I5/I7, I6 per-project (user excluded it).

### 2026-07-31 — C1 auto /rest/v1 API + OpenAPI shipped (Wave 2 begun); autonomous overnight run

User went to sleep, authorized autonomous completion of well-defined items + auto-merge of
verified PRs, with per-merge status reports. **C1 / item 123** (`742b355`): PostgREST-style
`/rest/v1/<table>` — GET/POST/PATCH/DELETE with eq/neq/gt/gte/lt/lte/like/ilike/in/is filters,
`select=`/`order=`/`limit`/`offset`. Injection-safe by construction: every value is a `$n`
bind through the existing `execute_sql_params_as_principal` path (RLS + table/column grants +
current_user/auth.uid/auth.jwt inherited, NOT re-implemented); identifiers catalog-validated +
quoted; operators from a fixed allow-list. **C3 OpenAPI also shipped** (`GET /rest/v1/` lists
tables → unblocks studio G4). Verified by me from clean: crash 54/54, server_rest **23/23**
incl. injection-treated-as-data, rls_parity_with_sql, column_grant_parity_with_sql,
no-privilege→403, unknown table/column→404, openapi-lists-tables; clippy/fmt clean. **Autonomous
plan:** complete + merge the well-defined engine/server items (this batch PR, then E1 realtime
auth, F1 storage per-object RLS, C2 FK-embed, maybe I4 migrations) — each a verified merged PR.
DEFER (need user decisions/secrets or too large, leave flagged): D OAuth/OTP/MFA/email, I6
per-project control plane, I7 edge functions, H SDK, C4 GraphQL, I2/I3/I5, G studio panels
(studio session's job). Verify-before-merge always (crash 54/54 + suites); anything ambiguous →
PR left open, not merged.

### 2026-07-31 — batch-2 tail: I1 rate-limiting + B5 column-level grants shipped (Workstream B complete)

Continued the post-#222 sequential run. **I1** (`5f7917d`): in-memory fixed-window auth
rate limiter (`src/server/rate_limit.rs`, `Instant`-based, `Arc<Mutex<HashMap>>`) over
`POST /auth/{login,signup,refresh}` only (keyed by IP+route+optional user); (max+1)th in a
window → `429 RATE_LIMITED` + `Retry-After`; both 200s and 401s count; `UNIDB_AUTH_RATE_LIMIT`
/`UNIDB_AUTH_RATE_WINDOW_SECS`. rate-limit tests 5/5, crash 54/54, auth suites unbroken.
**B5 / item 112** (`df62290`): column-level grants — `GRANT/REVOKE <priv> (cols) ON t`,
`GrantScope::All|Columns` (legacy grants deserialize as `All` — back-compat), enforced in
`check_plan_privileges` at PLAN time (pre-execution, zero per-row cost, so no fast path can
materialize an ungranted column): projection incl. `SELECT *` (requires-all), predicate/
GROUP BY/ORDER BY/HAVING, QuerySpec join/aggregate, UPDATE targets, INSERT lists, RETURNING;
**error-not-mask** (ungranted col → PERMISSION_DENIED, never NULL-filled); **policy-column
exemption** (columns only in RLS-injected predicates need no caller grant); `information_schema.
columns` filtered per grant; ambiguous unqualified join columns fail closed. item112 11/11,
item111 5/5, authz 33/33, crash 54/54. **Workstream B (122) is now fully shipped.** Verified
each by me from clean (`cargo clean` between builds; disk hit 97%). Remaining: **C1 — Wave 2
auto `/rest/v1` API** (the last queued item; unblocks studio G4), then a fresh PR to main.
Process: raise Bash `timeout` to 10 min for from-clean gate compiles; kill each agent's stray
background full-suite run (target-lock) before verifying.

### 2026-07-31 — PR #222 (items 121 A1–A4 + 122 B1–B4) MERGED to main; A5/A6 shipped to fresh branch

Merged the Supabase-parity auth PR #222 (squash `333f3d1`) into main — main now has the
full auth loop (password login/signup/refresh + `auth.uid()`/`auth.jwt()` + roles/role-scoped
policies). Reset the designated branch onto merged main and started the batch-2 tail + Wave 2
**sequentially** (disk-forced: usable ~38 GiB can't hold two concurrent ~30 GiB Rust builds —
the ENOSPC that thrashed batch 1; `cargo clean` between builds; raise the Bash `timeout` to
10 min for from-clean gate compiles rather than fighting the 2-min reaping). **121 A5/A6**
(`68332cf`): production JWT issuer (`UNIDB_JWT_SIGNING_KEY` — issuance no longer dev-login-only);
asymmetric verify (`UNIDB_JWT_PUBLIC_KEY` PEM, RS256/ES256 auto-detected) + public
`GET /.well-known/jwks.json` (empty `{"keys":[]}` in HS256-only mode; a test asserts the HS256
secret never leaks); asymmetric mode disables local HS256 issuance (documented); asymmetric
*issuance* deferred (verify-side only — key-management story out of scope). Verified by me:
crash 54/54, new item121_a5_a6_issuer_jwks 9/9, item121 16/16, item122 7/7, item100 9/9,
clippy/fmt clean; docs (REST_API/README/ops_runbook/backlog) updated by the agent. Studio told
it can now complete G1/G2/G3 against main's stable contract (G4 still waits on Wave-2 C1).
**Sequential queue remaining:** I1 rate-limiting → B5 column grants → C1 auto `/rest/v1` API,
then a fresh PR to main.

### 2026-07-31 — item 122 B3/B4 (built-in roles + role-scoped policies) shipped, second slice of Workstream B

Sequential batch-2 continuation (Sonnet on the main tree, verified by me). **122 B3/B4**
(`838d91d`): reserved `anon`/`authenticated`/`service_role` roles (CREATE/DROP reject them);
effective roles resolved engine-side via `RoleStore::effective_roles` — no subject→[anon],
verified subject→[authenticated]+transitive granted roles, `claims["role"]=="service_role"`→
[service_role] which BYPASSES RLS on the **audited** path (item-103) on BOTH writer and
concurrent-read. `CREATE POLICY … FOR <op> [TO <role,…>]`, target roles persisted
(serde-default, no FORMAT_VERSION bump). `apply_rls_with_auth` gains `roles`: **no-`TO`
policies still apply to everyone (back-compat)**, `TO`-scoped OR-combined only on role
intersection, table-with-only-scoped-policies-and-no-match denies all rows (fail closed).
Verified by me: crash 54/54, item122 7/7 (back-compat) + item122_b3_b4 6/6, rls/policy +
item121 16/16, clippy/fmt clean. Plan-time only, ACID/perf intact. Remaining on branch:
A5 prod issuer, A6 RS256/JWKS, B5 column grants; then `docs/REST_API.md` refresh + PR-on-request.

### 2026-07-31 — item 121 A3/A4 (signup + refresh tokens/sessions/logout) shipped, second slice of Workstream A

Built directly on branch (no worktree) on top of A1/A2 (`fcd320a`). **A3:**
`POST /auth/signup` creates a non-superuser with an argon2id credential via
`Engine::create_user_with_password` (thin wrapper over `RoleStore::apply`'s
existing `CreateUser` branch — no new DDL path needed); gated behind
`UNIDB_ALLOW_SIGNUP` (default off, 404 when disabled, mirroring the dev-login
posture); duplicate usernames reject with a clear `AUTHZ_ERROR`. **A4:**
`AuthState` gains a `sessions: BTreeMap<token_hash, SessionRec>` field
(`roles.json`, same control-plane store as credentials) — refresh tokens are
256-bit OS-CSPRNG opaque strings (NOT JWTs, via `argon2`'s already-vendored
`OsRng`/`RngCore`), and only their SHA-256 hash (new `sha2` dependency — fast
one-way hash is the right tool for an already-high-entropy secret, unlike
argon2id for passwords) is ever persisted. `POST /auth/refresh` verifies +
rotates (revoke-old + issue-new in one persisted step) — checks the JWT
signing key is available *before* touching session state, so a disabled
issuer can never revoke a still-good refresh token without handing back its
replacement. `POST /auth/logout` revokes idempotently. Unknown/expired/
revoked refresh tokens all collapse to one `401 INVALID_REFRESH_TOKEN`.
Login/signup responses gained `access_token`/`refresh_token`; `token` kept as
a deprecated alias of `access_token` (zero breakage to existing item100/
item121 tests). Manual `Debug` redaction extended to the new DTOs and
`AuthState::sessions` (mirroring A1's `credentials` redaction pattern).
**Evidence:** 15 new authz unit tests + 9 new item121 integration tests, all
green (16/16 total in that file); item100 9/9 unchanged; item122 7/7 +
server_authz 3/3 unaffected (checked as a courtesy, out of this workstream's
touch-points); crash 54/54; fmt/clippy clean (`--all-features --all-targets`).
Commit `77fab4a`. Stayed entirely off `logical.rs`/`executor.rs`/`parser.rs`
per the hard boundary with Workstream B. **Process note:** hit a host-level
disk-full condition mid-session (real usable capacity on this box is ~38 GiB
despite `df` reporting 252G nominal — the gap is pre-existing platform
overhead, not something this session's build caused alone, though the
`target/` dir's ~30 GiB of accumulated per-test-binary debug artifacts *was*
the proximate trigger); `cargo clean` recovered ~30 GiB and every gate was
rerun clean afterward. Remaining for Workstream A: A5 (production issuer,
explicit signing-key config) + A6 (RS256/JWKS).

### 2026-07-31 — Supabase-parity P0 pair (items 121+122 first slices) built via Sonnet agents + shipped to branch

User directed: implement the Supabase-parity P0 pair using Sonnet for implementation,
Opus orchestrating/reviewing, no ACID/perf compromises. Approach: **seam-first, then
parallel fan-out.** Step 0 (Sonnet, verified by me): `AuthPrincipal` seam widening the
RLS identity from `sub`-only to subject+claims+roles, carried to `ExecCtx` but inert —
crash 54/54, behavior-preserving (`58efa75`). Then two Sonnet worktree agents in
parallel: **A (item 121 A1/A2)** argon2id credential store + real password login
(passwordless→verified; unknown/wrong/no-credential all return one 401 with a dummy-hash
timing-parity verify; manual Debug redacts creds) — `fcd320a`; **B (item 122 B1/B2)**
`auth.uid()` + real `auth.jwt() ->> 'claim'` in RLS, substituted at injection time,
fail-closed. Integration review (me) caught a real bug in B: policies with `auth.jwt()`
returned 0 rows on the LIMIT/**QExpr** path because `apply_rls`'s `policy_sub` substituted
only `current_user` before the Expr→QExpr conversion — same shape as item-110. Fixed
(`2a5fe85`): `apply_rls_with_auth(plan,catalog,user,claims)` core + thin `apply_rls` shim;
`policy_sub` now substitutes auth context too. **All green: item121 7/7, item122 7/7
(incl. two-tenant isolation + LIMIT/QExpr + fail-closed), item100 9/9, crash 54/54,
clippy/fmt clean.** Pushed A+B+fix to the branch. Remaining slices (batch 2): A3/A4/A5/A6,
B3/B4/B5. Studio G-panels spun off to a separate session (`sagarm85/unidb-studio`).
**Process lessons (logged): run ONE main-tree cargo build at a time (concurrent gate
builds + agent worktree builds thrashed the target lock and a 25G worktree target filled
the disk to 98%); reclaim each agent worktree's `target/` immediately after integrating;
worktree agents share the object store so integrate by committing-in-worktree +
cherry-pick.** Auth/RLS work stayed off the storage hot path (plan-time substitution,
control-plane credentials) — ACID/perf intact by construction and by crash-harness proof.

### 2026-07-31 — Supabase-parity backlog filed (items 120–123) + studio G-panels plan

Planning/docs-only session (no engine code). User asked, via a WhatsApp-thread
screenshot, what unidb still lacks to be "like Supabase" and to file the pending
work as parallelizable specs. Audited the actual surface across BOTH repos
(`sagarm85/unidb` + newly-attached `sagarm85/unidb-studio`) rather than trusting
the README: verified token→RLS IS wired (`sub`→`CurrentUser`→`apply_rls`), that
`post_auth_login` is passwordless *identification* (no credential verify), that
storage already ships presign URLs + public/private buckets, and that unidb-studio
already ships every panel EXCEPT auth/policies/roles/API-docs (their backends don't
exist). Framing: unidb has the Postgres *security core*; "Supabase" is the BaaS
layer above it (auth service, `auth.uid()`/`auth.jwt()`/role-in-RLS, auto REST API).
Filed: **120** umbrella roadmap (workstreams A–I, P0 = auth core + RLS↔token +
auto-API, 3-wave parallel plan), **121** auth core (A), **122** RLS↔token binding
(B, folds parked item 112), **123** auto REST API (C). Registered in
`backlog_index.md` (next→124), added a "Supabase parity" note to Next-up. Matching
studio plan `docs/AUTH_POLICY_PANELS_PLAN.md` on the studio's mirror branch. Both
lints green (backlog OK 99 files; docs OK). Nothing implemented — awaiting the
user's go on which Wave-1 track(s) to build first (P0 critical path = 121 + 122).

> **Older session-log entries (2026-07-24 and earlier) live in
> [`docs/history/MEMORY_ARCHIVE_2026-07.md`](docs/history/MEMORY_ARCHIVE_2026-07.md).**
