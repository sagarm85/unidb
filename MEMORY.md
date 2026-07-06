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

- **Milestone:** **M1 — MVCC + CRUD is DONE** (all four checkpoints: M1.a
  MVCC core, M1.b SI abort-on-conflict, M1.c catalog + SQL subset, M1.d
  hardening + benchmarks). The project is SQL-queryable, transactional, and
  benchmarked end-to-end. M2 (vector & text search) has not been started.
- **State:** 112 unit tests + 10 crash-harness tests (P1–P9 plus the
  combined crash+MVCC property test) all green, `cargo clippy --all-targets
  -- -D warnings` clean, `cargo fmt --all --check` clean, release build
  succeeds. M1's benchmark table recorded in `PROGRESS.md`.
- **Immediate next task:** Start M2 planning (vector search: `VECTOR(n)`
  type, async background HNSW index, `NEAR` operator, full-text inverted
  index) — no M2 work has begun. Before that, two identified-but-deferred
  M1 items are worth a deliberate decision (fix now vs. carry forward, see
  Open questions below): the read-only-transaction fsync inefficiency found
  during M1.d's benchmark pass, and RC's EvalPlanQual re-evaluation path
  (D12), which remains unimplemented.
- **Last updated:** 2026-07-06

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
    logical.rs        — (new, M1.c) LogicalPlan/Expr/Literal/CmpOp + apply_rls (the entire
                         RLS mechanism is this one AND-rewrite function)
    parser.rs         — (new, M1.c) wraps `sqlparser`'s GenericDialect AST -> LogicalPlan
    executor.rs        — (new, M1.c) row-at-a-time executor; hand-rolled row encoding
                         (tag+value per column); no separate physical-plan IR (folded in)
  checkpoint.rs       — flush dirty → checkpoint WAL record → update control → truncate WAL
  recovery.rs         — (extended, M1.a) mini-txn redo/undo (unchanged) +
                         incomplete-user-txn undo pass (decodes ownership from WAL redo bytes)
  lib.rs              — Engine API: begin/commit/abort + insert/get/update/delete take an xid;
                         + execute_sql/set_rls_policy (M1.c); owns LockManager + Catalog
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

Nothing — M1.a, M1.b, and M1.c checkpoints all fully verified. Ready to
start M1.d (closing out the rest of M1's stated scope: combined crash+MVCC
property test, M1 benchmark table).

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

---

## Open questions / pending human input

- **Decide: fix the read-only-transaction fsync now, or carry it into M2?**
  (See the design note above and `PROGRESS.md`'s M1 entry.) It's a small,
  well-understood fix (skip `wal.commit_user_txn()`'s fsync in
  `TransactionManager::commit` when `Transaction.undo_log.is_empty()`), but
  touches the commit path CLAUDE.md's conventions would want treated as a
  deliberate change, not a drive-by — hence surfacing it here rather than
  just fixing it.
- **Decide: is catalog DDL's lack of transactionality acceptable to carry
  into M2, or does it need addressing first?** (See below.)
- Deferred-but-flagged for later milestones: slow-consumer-vs-vacuum durability
  contract (M4); filtered-HNSW vs over-fetch for RLS on `NEAR` (M2); SSI
  activation (post-M1, seam built in M1.a per D11, still all no-ops — M1.b's
  lock manager has no wait queue/deadlock detection, deliberately deferred to
  that future SSI effort).
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

- **Read-only transactions pay a full commit fsync for nothing** (found in
  M1.d's benchmark pass — see design note above). ~3,570x regression on
  point SELECT vs. M0, isolated entirely to this one unnecessary fsync.
  Straightforward fix identified, not applied — see Open questions above.
- FSM is a linear scan over all heap pages — fine for M0/M1, revisit if insert
  throughput regresses.
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

---

## Session log (append newest at top; use the real current date)

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

### 2026-07-06 — Project initialization
- Architecture design doc reviewed; six foundational gaps identified and resolved.
- Isolation decided: RC default / RR available / SSI seam now (D10–D12).
- Scope adjusted: single-file for M0 (D6); benchmark the replaced stack (§6).
- `CLAUDE.md`, `PROGRESS.md`, `MEMORY.md` created.
