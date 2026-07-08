# unidb Engine Design — M0 through M8

> Consolidated design document for the engine as shipped through M8
> (2026-07-08). Distilled from `CLAUDE.md` (locked decisions D1–D13),
> `MEMORY.md` (per-checkpoint design notes), `PROGRESS.md` (milestone
> entries + measured benchmarks), and the module doc comments in `src/`.
> M8 (attach client, §9) is included; see that section's note on how it
> was merged from a separate worktree, and §7.3's correction for a real
> M7 bug that merge verification found and fixed.
>
> Rules of the road: locked decisions live in `CLAUDE.md` §3 and require
> human sign-off to change; this document *describes*, it does not
> *authorize*.

---

## 1. Overview & design thesis

unidb is a single embedded storage/transaction engine in Rust that unifies
four data models over **one page store, one WAL, one buffer pool, one
transaction manager**:

1. **Relational CRUD** — SQL subset over MVCC-versioned heap tables.
2. **Vector search** — `VECTOR(n)` columns, HNSW index, `NEAR` operator.
3. **Graph** — edges with a Cypher read subset, adjacency indexes.
4. **Event queue** — durable event stream with Kafka-style consumer offsets.

A single transaction can touch all four atomically because there is one
node and one log. That is the competitive thesis: **eliminating the
multi-system dual-write tax**. "Save row + embedding + graph edge + event"
is one WAL append and one commit here, versus 3–4 network round-trips with
no shared transaction for Postgres + vector store + graph DB + Kafka.

**Explicit non-goals** (CLAUDE.md §1): no distributed consensus
(single-primary only), not full ANSI SQL (practical subset), no cloud
control plane. On single-model benchmarks against a specialized incumbent
the engine is expected to lose, and that is accepted — the benchmark that
matters is the cross-domain transactional workload against the replaced
stack (§11 below records what has actually been measured so far).

---

## 2. Architecture layer stack

```
API layer (M5)                REST + JWT(verify-only) + SSE + /metrics; embedded crate is primary
Query & execution (M1+)       SQL parser -> logical plan -> executor; Cypher subset (M3)
Logical record layer (M1+)    rows / vector records / graph edges / queue events
Transaction & concurrency     MVCC snapshots; write-write lock manager (abort-on-conflict)
Storage layer (M0)            single-file paged store; buffer pool; WAL; control file; recovery
```

Module map (what each layer became in code):

| Layer | Modules |
|---|---|
| Storage core (M0) | `format.rs`, `control.rs`, `mmap.rs`, `page.rs`, `bufferpool.rs`, `wal.rs`, `heap.rs`, `checkpoint.rs`, `recovery.rs` |
| Transactions (M1) | `mvcc.rs`, `txn.rs`, `lockmgr.rs`, `concurrency_hooks.rs` |
| Catalog & SQL (M1) | `catalog.rs`, `sql/{parser,logical,executor}.rs` |
| Secondary indexes (M2, M6, M7) | `index_worker.rs`, `vector.rs`, `fulltext.rs`, `btree_index.rs`, `csr_index.rs` |
| Graph (M3, M7) | `graph/{edges,index,logical,parser,executor}.rs` |
| Event queue (M4) | `queue/{mod,payload}.rs` |
| Server (M5, feature-gated) | `server/{engine_handle,error,dto,handlers,router,auth,sse}.rs`, `bin/unidb-server.rs` |
| Engine facade | `lib.rs` (`Engine` — the sole entry point) |
| Attach client (M8, separate workspace crate) | `unidb-attach/src/lib.rs` (`AttachClient`, `AttachError`) |

Two invariants shape everything above the storage layer:

- **The engine is synchronous and single-threaded by design.** The only
  background threads are the secondary-index worker (M2) and, when the
  `server` feature is on, the server's writer thread (M5) — and the writer
  thread *owns* the `Engine`, it does not share it. A default build has
  zero async dependencies (verified via
  `cargo tree --no-default-features --edges normal`).
- **Every operation takes an explicit `Xid`** from `Engine::begin` /
  `begin_with_isolation` and ends with `commit`/`abort`. There is no
  implicit transaction anywhere in the crate.

---

## 3. Storage core (M0)

### 3.1 On-disk format (D6, D8, D9)

- **Single data file** of fixed-size pages; the WAL is a separate file.
  Multi-file storage was deliberately rejected for now (D6) — it forces
  per-file LSN tracking into recovery for benefits (file placement,
  parallel backup) not yet needed.
- **Page size 8 KiB** by default, config-overridable at init only, baked
  into the control file (D8). Not changeable after files exist.
- **Everything on disk is little-endian** (D9). Every page carries a CRC32
  checksum and an LSN. No `serde` on the page/WAL hot path — page and WAL
  records are hand-rolled / `zerocopy`-style byte encodings for exact byte
  control (`serde_json` is used only for control-plane data; see §4.6).
- `FORMAT_VERSION` is currently **4**: v1→v2 for M1's tuple-header
  extension, v2→v3 for the `next_xid` control-file field (§4.7), v3→v4 for
  the `WAL_FPI` full-page-image record (P1.a torn-page protection, §3.3). No
  migration paths — no earlier version ever shipped externally.
- Pages use a **slotted-page** layout; tuples carry a 24-byte header with
  `xmin`/`xmax`/`prev_page`/`prev_slot` (reserved in M0 per D4, in active
  MVCC use since M1).
- `mmap.rs` (a thin `memmap2` wrapper) is the **only** module allowed
  `unsafe`; the rest of the crate denies it.

### 3.2 Control file (D3)

The recovery root — unidb's `pg_control`. 44 bytes: magic, format version,
`page_size`, last-checkpoint LSN, WAL tail pointer, `catalog_root` (page id
of the current catalog blob, M1), `next_xid` (bytes `[32..40]`, added in
the v3 bump), CRC32 (`[40..44]`). Created at DB init; **recovery always
starts here** — it is the single source of recovery truth.

### 3.3 WAL & mini-transactions (D1, D2, D5)

- **Buffer policy is steal + no-force, ARIES-style (D1)** — so the WAL
  carries both **redo and undo** information per record.
- **The atomic unit for a single statement is a mini-transaction (D2)**: a
  WAL-bracketed group of page writes (begin/commit records) that redo/undo
  treat as one. Every statement — including each statement inside a larger
  user transaction — is its own mini-txn with its own commit fsync. (This
  is the single largest measured performance cost in the engine; see §11.)
  **Update (2026-07-08, branch `m9-group-commit`):** a default-off
  `Wal::deferred_sync` mode lets a single serialized owner (the server
  writer thread) *append* mini-txn/user-txn commit records without fsyncing
  and force durability once per request batch (group commit) — see §11.1.
  The embedded path and the crash harness keep the per-statement fsync.
- WAL records use length-prefix framing (`u32` LE) plus a per-record CRC32;
  a recovery scan stops cleanly at the first corrupt/truncated record.
- **D5 — the invariant that must never break**: a dirty page may not be
  flushed or evicted while `page.LSN > durable_WAL_LSN`. Enforced in
  exactly two places: `bufferpool.rs::flush_page()` and the clock
  eviction's `find_victim()` path. This is a tested invariant (debug
  assertions + crash harness), not folklore.
- User transactions (M1) add `WAL_TXN_BEGIN`/`COMMIT`/`ABORT` record types
  sharing the mini-txn wire format — the `mini_txn_id` u64 slot doubles as
  the `Xid`. Mini-txn ids and xids are two independent ID spaces on one
  format. Later additive record kinds on the same wire format: `WAL_VACUUM`
  (M10, redo-only reclamation) and `WAL_FPI` (P1.a, below).
- **Torn-page protection (P1.a, `full_page_writes`).** An 8 KiB page write is
  not atomic; a crash mid-write leaves a half-old/half-new page that CRC
  detects but cannot repair — the #1 silent data-loss hole. On the **first
  modification of a page after each checkpoint**, the buffer pool logs the
  entire clean page image to the WAL as a redo-only `WAL_FPI` record
  (`BufferPool::maybe_log_fpi`, called from every `heap.rs` mutation before
  its first incremental record). Recovery replays the image as the clean base
  (`restore_page_image`, CRC-bypassing) and re-applies the interval's later
  incremental redos on top; a checkpoint's `clear_fpi_tracking()` re-arms the
  next interval. Correct with one image per page per interval because D5
  forbids flushing a page whose WAL isn't durable, so any torn on-disk page
  belongs to a committed mini-txn whose FPI is in the redo set. Cost: WAL
  grows (one 8 KiB image per page per interval — bounded by checkpoint
  frequency, hence auto-checkpoint P1.e); throughput unchanged (fsync-bound).
- **fsync-failure handling (P1.b, fsyncgate).** A failed `fsync`/`msync` may
  leave the OS having dropped the dirty data while clearing its dirty bit, so a
  retry can falsely succeed. `Wal` and `BufferPool` now latch a **poisoned**
  state on any durability-primitive failure and return `DbError::Durability
  Failure` for every later durability request — never falsely reporting
  durable. On failure the WAL does not advance `durable_lsn` and the pool does
  not mark the frame clean, so recovery still sees a consistent prefix; the
  session is unrecoverable and must restart. A `debug_assert!` at the eviction
  steal point re-verifies D5 (no page flushed ahead of the durable WAL). Fault
  injection: `Wal::arm_fsync_fault` / `BufferPool::arm_flush_fault` (crash
  point P12).

### 3.4 Buffer pool

Fixed capacity of **256 frames** (`POOL_CAPACITY` in `lib.rs`), pin/unpin,
clock eviction, dirty tracking, D5 enforcement on flush/evict.
`fetch_page` returns a per-call page copy — cheap for point access,
measurably expensive for hot-hub scans, which is what motivated the graph
layer's batch-latch optimization (§7.2). A real scaling limit was
discovered in M6: tables growing into the hundreds of pages could exhaust
the pool (`DbError::BufferPoolFull`) even with small individually-committed
transactions.

**Steal / force-WAL-on-evict (2026-07-08, branch `m9-group-commit`).** The
pool tracks the durable WAL frontier (`durable_wal_lsn`, a stale-low-safe
lower bound refreshed on every write-path fetch and on `Engine::sync_wal`).
`find_victim` now writes back and evicts a dirty page once its LSN is
durable (proper ARIES steal) instead of only ever evicting clean frames —
which is why the M6 `BufferPoolFull` limit existed (the old D5 hint was
hardwired to `INVALID_LSN`, so no dirty page was ever a victim). The
write-path entry point `BufferPool::fetch_page_for_write(page_id, &mut Wal)`
(used by every heap write/undo path and the FSM scan) refreshes the frontier
and, if the pool is still full of *not-yet-durable* dirty pages (the
group-commit deferred-sync case), forces one `Wal::sync()` and retries —
"force the log before stealing the page" (D5). Reads keep plain `fetch_page`
(they never dirty pages). This makes deferred mode safe for working sets
larger than the pool and largely closes the M6 limit; the linear-scan FSM
cost is separate and still open (§12).

### 3.5 Checkpoint & recovery

`checkpoint::run`: flush all dirty pages → write a checkpoint WAL record →
persist control file (checkpoint LSN, WAL tail, `catalog_root`,
`next_xid` captured *before* truncation) → truncate the WAL before the
checkpoint LSN (currently by rewriting the whole file — tracked debt).

`recovery.rs` on open: read control file → **redo** from the checkpoint
LSN → **undo** incomplete mini-txns → undo incomplete *user* transactions.
The user-txn undo pass reconstructs write ownership by decoding
`xmin`/`xmax` straight out of the WAL's redo bytes (no in-memory state
survives a crash), in two phases: revert xmax-stamps first, then
force-self-stamp inserts last — so a row both inserted and superseded by
the same aborted transaction ends up dead, not revived. The pass is
idempotent, which is what makes crash-*during*-abort safe.

### 3.6 Crash-injection harness (D7)

`tests/crash/main.rs` — kill at defined points, reopen, assert the
recovered state equals the expected committed set. Points P1–P5 (M0:
post-WAL/pre-flush, mid-checkpoint, post-mutation/pre-commit, during WAL
truncation, post-commit-fsync), P6/P7 (M1: user-txn boundaries), P9 (M1:
crash mid-undo of an aborting transaction), P10 (M10: crash mid-vacuum),
P11 (P1.a: torn 8 KiB page write recovered from its `WAL_FPI` image + redo),
P12 (P1.b: WAL/data-file fsync failure refuses to report success and latches
poisoned), plus a property test running random `BEGIN`/`INSERT`/`COMMIT`/
`ROLLBACK` sequences with random crash points — recovered state must be exactly
the set of transactions that reached `WAL_TXN_COMMIT`. **14 crash tests as of
P1.b.**
Deliberately *not* a deterministic simulator (no TigerBeetle/FoundationDB-grade
sim).

The harness is required to **grow** whenever a new durability mechanism lands:
M1–M8 added none (edges, events, and consumer offsets are all ordinary
WAL-backed heap rows covered by the existing machinery, and every secondary
index is derived, rebuildable, WAL-free state whose loss on crash is expected,
§10), but M10 added P10 for the new `WAL_VACUUM` path, Phase 1's P1.a added
P11 for the new `WAL_FPI` torn-page path, and P1.b added P12 for the
fsync-failure (poison) path.

---

## 4. Transactions & MVCC (M1)

### 4.1 Visibility

`mvcc.rs` holds pure snapshot logic: a `Snapshot` (active-xid set + range)
and `is_visible(tuple, snapshot)`. There is deliberately **no "aborted"
state** in visibility: `is_committed_at_snapshot` treats
not-in-active-set-and-in-range as committed. Abort therefore requires
**physical undo** (§4.3).

### 4.2 Isolation levels (D10, D12)

- **READ COMMITTED** (default) and **REPEATABLE READ** (= snapshot
  isolation) differ *only* in snapshot lifetime: per-statement vs
  per-transaction. Same MVCC machinery.
- **Conflict handling is SI's abort-on-conflict path (D12)**: a
  write-write conflict aborts the second writer immediately — no blocking,
  no wait queue, no deadlock detection (deliberately). RC's
  EvalPlanQual-style re-evaluation path is designed but **unimplemented**;
  UPDATE/DELETE conflicts surface as `WriteConflict` at every isolation
  level (tracked gap, §12).
- **The `on_read()`/`on_write()` seam (D11)** exists in every scan and
  lookup path (`concurrency_hooks.rs`), all no-ops — so a future
  SERIALIZABLE/SSI is an *addition* (read-set tracking behind the seam),
  not an executor rewrite.

### 4.3 Writes, locks, abort

- `Heap` insert/update/delete write MVCC versions: UPDATE always creates a
  new `RowId` and stamps the old version's `xmax` (never in-place);
  `prev_page`/`prev_slot` record version history for recovery/vacuum
  bookkeeping only — **readers never walk the chain**. There is **no
  cross-statement `RowId` stability**: callers must use returned `RowId`s
  or re-scan; `Heap::get` does a single direct visibility check.
- `lockmgr.rs`: write-write locks only (no read locks under MVCC), keyed
  by `RecordId::row(page_id, slot)` packed into a u64. Because `PageId` is
  allocated from one global `BufferPool` counter (not per-table), lock keys
  are globally unique across all tables — this is why graph edges (M3)
  needed zero new locking code. Locks are held for the whole transaction,
  released only at commit/abort — which is also why no separate
  "commit-time recheck" is needed: there is no window between write and
  commit for another writer to slip through. `Heap::update`/`delete` run
  two checks that together are the complete conflict detection: (1)
  `try_acquire_write` catches a *currently active* competing xid;
  (2) the `xmax != 0` check catches a row superseded by a since-committed
  transaction. Both return `WriteConflict` by design.
- **Abort = physical self-neutralization**: self-stamp `xmax := own xmin`
  on every tuple the transaction inserted, revert every xmax-stamp it
  applied, driven by an in-memory `Vec<UndoAction>` per transaction.
  `Heap` never records undo itself — every call site pairs the heap call
  with an explicit `txn_mgr.record_undo(...)`, which is what let later
  milestones (edges, events) inherit correct abort behavior with zero new
  abort-path code. The corresponding risk (a forgotten `record_undo` is a
  *silent* visibility bug) is guarded by per-feature abort-visibility
  tests (§10).

### 4.4 Transaction manager

`txn.rs`: begin/commit/abort, RC-vs-RR snapshot lifetime, lock release,
xid issuance. **Correction (2026-07-08, branch `m9-group-commit`):**
`commit()` previously fsynced unconditionally — *including for read-only
transactions*, which was the single most visible latency bug in the
benchmarks (point SELECT went 855 ns in M0 → 3.05 ms in M1 from this one
fsync). **Now fixed:** `commit()` skips `commit_user_txn` (WAL record +
fsync) entirely when `undo_log.is_empty()`. Point SELECT is back to
~1.09 µs. Safe because recovery treats the orphan `WAL_TXN_BEGIN` as an
incomplete user txn whose undo pass finds no mutations to reverse.

### 4.5 The xid-reuse-after-checkpoint bug (fixed, control file v3)

`recover_next_xid` resumed the xid counter by scanning the WAL for
`WAL_TXN_BEGIN` — correct only while those records still exist, but
checkpointing truncates them, so the first open after any checkpoint
silently reset the counter to 1. **Silent-corruption class**: a reissued
xid can collide with `xmin`/`xmax` values on existing tuples, producing
wrong query results with no error. Found by manually smoke-testing M5's
REST server (the first code path ever to combine commit → `checkpoint()`
→ reopen). Fix: persist `next_xid` in the control file at every
checkpoint (captured before truncation); `Engine::open` resumes at
`max(WAL-scan, control.next_xid)`. D3/D9 change with explicit human
sign-off, recorded in `PROGRESS.md`.

### 4.6 Catalog

`catalog.rs`: `TableDef`/`ColumnDef`/`ColumnType`
(`Int64`/`Text`/`Bool`/`Json`/`Vector(n)`/`Decimal(p,s)`/`Timestamp` — the last
two added in Phase 2 P2.a), persisted as a single
`serde_json` blob rewritten to a fresh page on every change, pointed at by
`control.catalog_root`. Using `serde` here is deliberate — schema is
infrequent control-plane data, not the hot path D9 protects.
`TableDef.pages: Vec<PageId>` persists each table's page list; the
executor reconstructs a `Heap` handle per statement via
`Heap::from_pages` (cheap; avoids a cache-invalidation story).

**Catalog is not MVCC-versioned**: DDL takes effect immediately and
globally; `CREATE TABLE` inside a transaction that later aborts is *not*
rolled back. A real, narrow, tracked correctness gap (§12). Each catalog
rewrite also leaves the previous blob's page behind (same
no-vacuum story as the heap).

### 4.7 SQL subset

`sql/parser.rs` wraps `sqlparser` 0.62 (`GenericDialect`) into a
`LogicalPlan`; `sql/executor.rs` executes row-at-a-time (no separate
physical-plan IR — the grammar maps 1:1). Supported: `CREATE TABLE`,
`INSERT`, `SELECT` (star/named projection, **AND-only** `WHERE`),
`UPDATE`, `DELETE`, `CREATE INDEX ... USING HNSW|FULLTEXT|BTREE` (the
`USING` clause precedes the column list — a `sqlparser` grammar fact),
`NEAR(column, [..], k)` inside `WHERE`. JSON columns support `->`/`->>`
(note: they bind looser than `=` under `GenericDialect`; parens required).
Not supported (deliberate scope, parked in
`docs/backlog/phase2_sql_capability_expansion.md`): `OR`, `ORDER BY`,
`LIMIT`, aggregates, joins, subqueries, `IN (...)`.

**Constraints (M11, `sql-constraints` branch — pending merge):** `CREATE
TABLE` column options and table constraints — `PRIMARY KEY`, `FOREIGN KEY` /
`REFERENCES`, `UNIQUE`, `NOT NULL`, `CHECK`, `DEFAULT` — are parsed into
`ColumnConstraints`/`TableConstraints` on the catalog and enforced on
INSERT/UPDATE. DEFAULT fills a NULL at INSERT; NOT NULL / CHECK are per-row
checks (CHECK reuses `eval_expr`, so it inherits two-valued NULL semantics);
UNIQUE is a **synchronous heap scan** under the writer's snapshot (not the
async B-Tree index, which can be stale — the M7 lesson); FK enforces
referenced-table existence only. See `PROGRESS.md`'s M11 entry.

**RLS is a planner rewrite**: `apply_rls` ANDs the stored policy predicate
onto `Select.predicate`. That one function is the whole mechanism; it
composes for free with `NEAR` and index-assisted paths because they all
filter through the same `predicate_matches`. RLS is Rust-API-only
(`set_rls_policy`) — no SQL or REST surface, since `Expr` has no untrusted
serialization design.

Row encoding is hand-rolled tag+value per column (that *is* the hot path):
tags 0=`Null`, 1=`Int64`, 2=`Text`, 3=`Bool`, 4=`Json`, 5=`Vector`
(`[dim: u32 LE][f32 LE × dim]`, dimension-prefixed so `decode_row` can
cross-check the schema), 6=`Decimal` (`[i128 LE (16 B)][scale: u8]`, P2.a),
7=`Timestamp` (`[i64 LE micros]`, P2.a). New tags are additive and
forward-compatible (D4) — old rows never carry them, so no `FORMAT_VERSION`
bump. Vector dimension (and, for decimals, the stored scale) is validated in
three independent places (DDL, INSERT/UPDATE coercion, every decode), each
guarding a different failure mode.

---

## 5. Secondary indexing framework (M2 + M6)

### 5.1 The async index worker

`index_worker.rs` — the engine's first background thread. It owns exactly
one thing: `Arc<RwLock<HashMap<(table, column), IndexEntry>>>`, built
purely from `IndexMsg` channel messages. **It never receives a
`BufferPool`, `Wal`, or `Heap` handle** — the load-bearing isolation
decision. Two flows share one FIFO channel:

- **Rebuild-on-open**: `Engine::open` runs an ordinary read-only MVCC scan
  of committed rows, sends one `Upsert` per indexed value, then one
  `MarkReady` per column. FIFO ordering is what makes
  `IndexStatus::Building` → `Ready` meaningful. (`MarkReady` on a
  never-upserted column creates an empty *Ready* entry — a real stuck-in-
  `Building` bug found and regression-tested in M2.d.)
- **Live upserts**: the SQL executor sends per-row messages from
  `exec_insert`/`exec_update` for indexed columns only — zero cost for
  unindexed tables. "Row write is the only synchronous cost" (with the
  honest asterisk that the worker's CPU isn't free; §11).

`CREATE INDEX` validates kind-vs-column-type, persists via
`Catalog::set_column_index`, and backfills committed rows immediately;
the shared `build_indexed_columns` is the single place mapping
`ColumnType`/`IndexKind` → indexed value, used by DDL backfill, live
upserts, and rebuild-on-open alike.

### 5.2 Index kinds

| Kind | Structure | Notes |
|---|---|---|
| `Hnsw` (M2) | `vector.rs` wrapping `instant-distance` | **No incremental insert exists in the crate's public API** (verified against vendored source), so `VectorIndex` buffers all points and rebuilds the whole graph per upsert — O(n log n) per insert, off the foreground thread but real CPU (§11, §12). f32, Euclidean (pgvector `<->` convention). |
| `FullText` (M2) | `fulltext.rs` `InvertedIndex` | Whitespace+lowercase tokens, AND-only multi-term intersection. **No SQL query surface** — only the Rust API; `NEAR` is vector-only. |
| `BTree` (M6) | `btree_index.rs`, `std::BTreeMap<OrderedValue, Vec<RowId>>` + `by_id: HashMap<RowId, OrderedValue>` | The `by_id` reverse map is what lets `upsert` remove a stale value-bucket entry — new bookkeeping the value-keyed structure needs that the id-keyed M2 indexes didn't. Zero new dependencies. |
| `Csr` (M7) | `csr_index.rs` | **Engine-managed only** — no SQL surface; registered as `("__edges__", "from_id")` purely to reuse the worker machinery. See §7.3. |

### 5.3 Query execution against indexes

Both index-assisted paths use the same **resolve-then-refilter template**:
index → candidate `RowId`s → `Heap::get` under the caller's snapshot
(drops `NoVisibleVersion`) → full `predicate_matches` (so remaining AND
terms + RLS apply identically to a full scan).

- **`NEAR` (M2.d)**: over-fetch `max(4k, k+20)` candidates, then filter.
  Requires an HNSW index (clear `SqlPlan` error otherwise — no silent
  full-scan fallback, because approximate top-k has no correct fallback).
  An index still `Building` yields a *partial* (never wrong) result set —
  acceptable for inherently approximate top-k.
- **B-Tree (M6.b)**: `find_indexable_btree_predicate` detects a top-level
  or AND'd `Column <op> Literal` on a BTree-indexed column. **Correctness-
  critical difference from `NEAR`**: the index is only trusted once
  `Ready` — an equality/range query silently missing rows during backfill
  would be a real bug — so anything short of `Ready` falls back to the
  unchanged full scan. First indexable term wins; there is no cost-based
  selection (§12).

---

## 6. Event queue (M4)

### 6.1 Why not tail the WAL

The obvious design — a consumer tailing the live WAL — is a dead end for
two independent, source-verified reasons: (1) `checkpoint::run` truncates
the WAL unconditionally, with no reader registry, and making WAL retention
depend on external readers would be D5-adjacent bad news; (2) WAL records
carry no table identifier (only `page_id`/`slot`), so a raw-WAL consumer
couldn't even tell whose row it's reading.

### 6.2 The actual design: events are ordinary rows

`send_event_capture` (SQL executor) copies each triggering row into a
durable `__events__` heap table **inline, at write time, under the writing
transaction's own xid** — the same "just an ordinary system table" trick
as `__edges__`. Consequences, all structural rather than promised:

- WAL truncation *cannot* care about consumer lag — the event no longer
  lives only in the WAL. Proven by a test that checkpoints five times
  under a never-acking consumer and still polls every event.
- Events get MVCC, WAL durability, abort semantics, and
  `SELECT * FROM __events__` queryability for free. An aborted
  transaction's events vanish with it (the capture is inline under the
  same xid — a commit-time hook was explicitly rejected because it creates
  a window where event and data-commit can disagree).
- Consumer offsets live in `__consumers__`, updated by `ack_events` under
  the caller's transaction — an aborted ack does not advance the offset
  (tested).

`poll_events`/`ack_events` are Kafka-style manual-commit;
`vacuum_events()` reclaims rows acknowledged by **every** registered
consumer (`min(offsets)`), and is the only current lever bounding
`poll_events`'s cost, which scales linearly with `__events__`'s total size
(no predicate pushdown / no `seq` index yet — measured, §11). Nothing
calls vacuum automatically.

---

## 7. Graph (M3 + M7)

### 7.1 Edges are ordinary rows

Edge records `(from_id, to_id, edge_type, props)` live in a synthetic
`__edges__` system table. The headline M3 finding: this needed **zero new
storage-layer or locking code** — lock keys are already globally unique
across tables (§4.3), so per-edge locking, first-committer-wins, and
release-on-commit/abort were all inherited and then proven by
`tests/graph_locking.rs` rather than assumed. Nodes are opaque `i64` IDs
— no labels, no property-graph joins (rejected at parse time, not
mis-parsed).

### 7.2 Read path: EdgeIndex + batch-latch resolution

`EdgeIndex` (by `from_id`) is maintained **synchronously** inside
`create_edge`/`delete_edge` — always current the instant a call returns.
It has no abort-time cleanup; stale entries are permanently safe because
every candidate is re-validated against the caller's snapshot (§10).

`resolve_candidates_batched` groups candidate `RowId`s by `page_id` so a
hot hub costs one `fetch_page` per page instead of one per edge (~128
edges/page): measured ~9.3–9.7x over naive resolution, and it closes
almost the whole read-side gap with Postgres (94.3 µs vs 98 µs at 1k
edges; 930 µs vs 568 µs at 10k).

### 7.3 CSR index (M7)

A read-optimized adjacency structure built asynchronously on the existing
index worker, sitting **alongside — never replacing —** the synchronous
`EdgeIndex`. `CsrIndex` splits `stage()` (append to a raw `Vec`) from
`rebuild()` (recompute sorted `from_ids_sorted`/`row_ptr`/`col_ind` — the
classic CSR arrays, O(n log n), not incrementally patchable). Unlike
HNSW's rebuild-per-upsert, CSR's rebuild is **debounced**: the worker
drains every queued message via `try_recv()` before one `rebuild_dirty`
pass, coalescing write bursts (tested: 200 messages → far fewer rebuilds).

**Tier selection — corrected during M8 merge verification, not shipped as
originally designed.** The original design had `edges_from`/Cypher prefer
CSR once `Ready`, falling back to `EdgeIndex` otherwise, on the reasoning
that CSR's async lag can only cause a *false negative* (missed
very-recent edge), never a phantom — safe, since MVCC re-validation
downstream catches everything else. **That reasoning missed a specific
case**: `Ready` means "the initial backfill completed" (true almost
instantly for a fresh/empty table), not "every edge write since then is
reflected in the debounced rebuild." A transaction's own just-created edge
could therefore be invisible to its own immediate `edges_from` call —
a same-transaction self-visibility miss, not a merely-stale read, and a
real regression against the guarantee `edges_from` had always given since
M3. Found via `cargo test -p unidb --test graph_mvcc
aborted_edge_creation_never_surfaces_in_traversal`, reproduced 30/30 times
in isolation (masked when run as part of the full workspace test suite).
**Fixed**: `edges_from`/Cypher now call `EdgeIndex::candidates` directly
and unconditionally, exactly as before M7. `CsrIndex` itself is
unaffected — still built, kept warm by every live edge write, and
rebuilt on open — it simply isn't consulted by any query path today. A
correct fix would need a staleness/generation marker proving CSR has
incorporated every write up to a specific point before a caller can trust
it; not attempted, since reverting the bug was the scope, not designing
new correctness machinery. `delete_edge` sends no CSR message; deletion
would be implicit via MVCC filtering if CSR were ever consulted again.

Measured result, reported plainly: even when it was wired in, CSR was at
**parity** with EdgeIndex+batched-resolve on today's single-hop
workloads — batched page resolution dominates, and binary search ≈
HashMap lookup at that point. CSR's real payoff (cache-friendly contiguous
adjacency for multi-hop traversal) is headroom Cypher can't exercise yet,
independent of the tier-selection bug above.

### 7.4 Cypher subset

`MATCH (a)-[:TYPE]->(b) WHERE ... RETURN ...` — exactly one fixed-length
directed hop, read-only (no `CREATE`/`DELETE`; mutations are Rust-API
`create_edge`/`delete_edge`). The `:TYPE` filter and `WHERE` predicate are
AND'd into one predicate applied through the SQL layer's own
`predicate_matches`/`eval_expr` (promoted to `pub(crate)` — the only SQL
change M3 needed), so index fast path and full-scan fallback filter
identically. `ExecCtx` stays graph-free; graph state is passed as extra
arguments (Rust's disjoint field borrows make this clean).

---

## 8. Server (M5)

Everything server-side is behind the `server` Cargo feature; a default
build has zero tokio/axum/jsonwebtoken in its dependency graph — "the
engine stays sync" is literally true for the shipped artifact.

**The core decision: async handlers never touch `Engine`.** One dedicated
OS thread (`server/engine_handle.rs`) owns the `Engine` for its whole
life; handlers send typed requests over an mpsc channel and await a
per-request oneshot reply. Chosen over `Mutex<Engine>` to preserve the
engine's real invariant (single-thread ownership) instead of imposing a
"never `.await` while holding the lock" discipline on every future call
site. `EngineHandle::spawn` opens the `Engine` synchronously on the
caller's thread so open failures surface as an immediate `Err`.

Surface (see `docs/REST_API.md` for contracts): `POST /sql` (atomic
`;`-separated multi-statement transactions — free, since `execute_sql`
already runs a whole string under one xid), `POST /cypher`, raw row CRUD
(`/rows...`), graph routes (`/edges...`), indexing
(`POST /indexes`, status), events (`POST /tables/{t}/events`,
`GET /events/subscribe` SSE, `POST /events/ack`), `POST /checkpoint`,
`GET /metrics` (Prometheus), `POST /txn/begin` (introspection only — no
multi-request transaction sessions exist). Auth is **verify-only JWT**
(HS256 Bearer; the server never issues tokens, has no user store). SSE is
**server-polls-then-pushes** (`poll_events` has no wake primitive), which
is why subscriber cost scales badly (§11). No TLS (assumed behind a
reverse proxy); no admin-scope claims; no RLS surface.

---

## 9. Attach client (M8)

A third deployment mode alongside embedding `Engine` directly or running
the standalone REST server: `unidb-attach`, a Rust crate giving one-shot,
`Engine`-like method calls to a process that isn't running its own
`Engine`, built entirely on the REST surface described in §8 — no new
protocol, no new server-side capability.

**Workspace, not a nested subdirectory move.** The repo root `Cargo.toml`
does double duty as both `[workspace] members = ["unidb-attach"]` and
`[package] name = "unidb"` in the same file — `src/`, `tests/`, `benches/`
all stay exactly where they were pre-M8. This keeps `reqwest` and its
dependency tree completely out of the embedded `unidb` crate (it's a
`unidb-attach` dependency only, confirmed via `cargo tree -p unidb
--no-default-features --edges normal`), while avoiding a disruptive
file-move migration.

**One call = one complete operation**, not a mirror of embedded `Engine`'s
explicit `begin`/op/`commit` shape. There is no multi-request transaction
session over HTTP — every mutating REST route already does its own
internal begin→execute→commit (§8). Multi-statement atomicity is
available via `;`-separated SQL passed to `execute_sql`, exactly as REST
already supports. This is a deliberate, documented API-shape difference
from embedded `Engine`, not an oversight.

`AttachError`, not `DbError`, is the client's error type — `DbError`'s
variants are storage-internal (`PageNotFound`, `ChecksumMismatch`, ...)
with no meaningful mapping from an HTTP response. `AttachError` instead
mirrors the server's documented `code` field 1:1 (`TableNotFound`,
`ColumnNotFound`, `NotFound`, `TableAlreadyExists`, `WriteConflict`,
`SerializationFailure`, `SqlParse`, `SqlPlan`, `SqlUnsupported`) plus
transport-level variants (`Http`, `Json`, `InvalidToken`) and a generic
`Api { status, code, message }` catch-all.

Blocking `reqwest::blocking::Client`, no tokio runtime, no background
thread — one call blocks its calling thread for one HTTP round-trip; the
crate depends on `unidb` only as a `dev-dependency` (shared DTO shapes for
its own integration tests, which spin up a real `unidb-server`), so a
production consumer of `unidb-attach` never pulls in the embedded engine's
dependency graph. Rust-only in v1; `vacuum_events`/`set_rls_policy`/`flush`
are not exposed since the server itself has no REST route for any of the
three (§8) — tracked in `docs/backlog/`, not silently dropped.

Benchmarked (`unidb-attach/benches/attach.rs`) against a hand-rolled raw
`reqwest` call and a direct embedded `Engine` call for the same
`execute_sql` operation: the client wrapper tracks raw `reqwest` closely
(no meaningful overhead beyond what HTTP itself costs), both an order of
magnitude slower than the embedded call — the same HTTP-vs-embedded
finding §11 already establishes for the server, not a new tradeoff M8
introduces.

---

## 10. Correctness strategy (the recurring pattern)

Three rules repeat across every milestone and are worth stating once:

1. **Secondary indexes are non-transactional, non-durable, derived
   state.** None of them (HNSW, inverted, B-Tree, EdgeIndex, CSR) has
   abort-time cleanup or WAL presence. Correctness rests entirely on:
   *every candidate an index produces is re-checked against the caller's
   MVCC snapshot (and full predicate) before becoming a result.* Stale or
   phantom entries are therefore space leaks, never wrong answers. Each
   milestone lands a dedicated proof-of-abort-invisibility test
   (`vector_mvcc`, `graph_mvcc` ×2, `btree_mvcc`, `queue_mvcc`) written
   *next to* the feature, not deferred — because the failure mode (a
   forgotten `record_undo`, a worker with no transaction concept) is
   silent.
2. **New durable state is always ordinary heap rows** (`__edges__`,
   `__events__`, `__consumers__`) — inheriting MVCC, locking, WAL, crash
   recovery, and abort handling with zero new mechanism. This is why M3–M5
   added no new crash-injection points: there was no new durability
   machinery to crash.
3. **Completeness-sensitive vs approximate readers must gate on index
   readiness *and* on how current the index can ever promise to be.**
   B-Tree (exact results) falls back to a full scan unless `Ready`; `NEAR`
   (approximate top-k) accepts partial results while `Building` — both
   correct because their contracts already permit "may return fewer
   results while not yet caught up." **CSR (graph) was originally given
   the same treatment (prefer-once-`Ready`) and that was wrong** — see
   §7.3's correction. `edges_from`/Cypher have never had a "may return
   fewer results" contract; they've always guaranteed immediate
   self-visibility of a just-created edge, which `Ready`-gating alone
   cannot preserve for a debounced-rebuild structure. The lesson: rule 3
   only holds for readers whose *contract* already tolerates staleness,
   not for every reader that happens to sit behind an async index. Rule 1
   above (MVCC re-validates every candidate) rules out *wrong* answers;
   it does not by itself rule out *missing an answer the caller was
   promised*, which is a distinct, contract-specific question rule 3 must
   answer for each reader individually.

Test inventory (post-M8, `-p unidb` default features): 225 unit tests, 11
crash-harness tests, plus per-domain integration suites (`graph_locking`
4, `graph_rebuild` 3, `graph_mvcc` 2, `index_rebuild` 5, `vector_mvcc` 1,
`btree_mvcc` 1, `queue_vacuum` 4, `queue_mvcc` 2) = 258 total; add 25
`server_*` integration tests with `--features server` (228 unit tests, 3
of them feature-gated). `unidb-attach` adds 19 integration tests (3 CRUD +
6 extras + 4 graph + 6 SQL) + 1 doctest. Clippy `-D warnings` and fmt
clean across the whole workspace. `graph_rebuild`/`graph_mvcc` counts
dropped from M7's peak (5/3) back to their pre-M7 levels (3/2) when the
CSR-preferring tests were removed alongside the bug fix — no coverage was
lost, since the underlying `EdgeIndex` path was already covered by the
tests that remain.

---

## 11. Performance profile (measured, not aspirational)

All numbers from `PROGRESS.md` (release builds, Apple Silicon macOS, real
fsync). Baselines per CLAUDE.md §6: SQLite for M0/M1 (honest embedded
analog), Postgres+extension proxies from M2 on. The full four-system
"replaced stack" benchmark is possible since M4 but remains a deferred,
dedicated follow-up.

### 10.1 The one bottleneck that explains almost everything

**Every statement pays its own WAL fsync (~3 ms on this hardware)** —
D2's per-statement mini-txn, unchanged since M0, with a second
per-transaction commit fsync on top since M1. Consequences observed
independently in every milestone:

- Single-statement-transaction INSERT/UPDATE: ~155–165 ops/s (M1) — ~30x
  behind SQLite, ~35x behind Postgres, and *flat* ever since.
- Read-only transactions ~~pay a commit fsync **for nothing**~~ **— FIXED
  2026-07-08 (branch `m9-group-commit`)**: point SELECT 855 ns (M0) →
  3.05 ms (M1) → **~1.09 µs** now that `commit()` skips the fsync when
  `undo_log.is_empty()`.
- Server throughput ceiling: `POST /sql` was flat ~135→157→158 ops/s
  across 1/10/50 concurrent clients — the single-writer thread + per-op
  fsync, not HTTP (the HTTP/writer-thread layer itself costs only ~6%).
  **Update 2026-07-08 (branch `m9-group-commit`):** group commit (below)
  lifts this to **~242 → ~756 → ~4,780 ops/s** at 1/10/50 clients —
  throughput now *scales* with concurrency instead of being flat.
- `vacuum_events`: ~3.06–3.10 ms/row *regardless of batch size* — each
  reclaimed row is its own fsyncing mini-txn. (Not yet run under group
  commit; the mini-txn fsync it pays is now deferrable in the same way.)
- Batching statements inside one user transaction amortizes the
  *user-commit* fsync but not the per-statement one: 100-row single-txn
  INSERT still ~3.45 ms/row (M4) vs Postgres ~0.062 ms/row.

~~Group commit / WAL batching has never been scheduled~~ **Group commit
landed 2026-07-08 (branch `m9-group-commit`, server writer thread) — see
`docs/backlog/group_commit_and_read_concurrency.md`.** A default-off
`Wal::deferred_sync` mode lets the single-owner server writer thread append
all commit records for a drained request batch and force durability with a
single fsync, withholding each commit's reply until that fsync so no client
observes a non-durable commit. This is the prerequisite for the
cross-domain workload being fast too. Buffer-pool force-WAL-on-evict landed
alongside it (§3.4) — the pool now forces the log before stealing a
not-yet-durable dirty page, so deferred mode is safe for working sets larger
than the pool. One follow-up remains: a concurrent read path (readers off
the single writer thread).

### 10.2 Where the engine is genuinely competitive

- **Graph adjacency reads**: batched scan 94.3 µs vs Postgres 98 µs (1k
  hot hub); 930 µs vs 568 µs (10k) — the batch-latch optimization closed
  a 9–16x gap to ~1x–1.6x.
- **`poll_events`**: 20.8 µs–983.7 µs (100–5,000 rows) vs Postgres
  SKIP-LOCKED cycle ~2.6–3.1 ms — though unidb's cost grows linearly with
  table size while Postgres's partial index stays flat; `vacuum_events`
  is currently the only lever.
- **B-Tree-assisted SELECT scales flat** (~3.1 ms at 1k and 10k rows)
  while full scans grow — the absolute number is fsync-dominated, the
  scaling win is real.
- **JWT verification ~817 ns** — auth is nowhere near the cost.

### 10.3 Known performance liabilities (beyond the fsync)

- **HNSW rebuild-per-upsert** (no incremental insert in
  `instant-distance`): index-active INSERT already 2.8x slower at 200
  rows; scales as O(n log n) per insert. CSR has the same non-incremental
  shape but debounced (frequency reduced, structure cost unchanged).
- **SSE subscriber scaling**: 1→10→50 subscribers is 5.2→33.9→162.6 ms —
  N pollers × poll interval × linear `poll_events`, all serialized
  through the one writer thread.
- **`NEAR` latency (~4–5 ms)** is transactional overhead, not vector
  search — the raw structures answer in microseconds (fulltext search
  ~14.2 µs).
- **`BufferPoolFull` at ~100k rows/table** (M6 discovery): **largely fixed
  2026-07-08** — the root cause was that `find_victim` could never evict a
  dirty page (its D5 hint was hardwired to `INVALID_LSN`); it now writes back
  + evicts dirty pages once durable, and `fetch_page_for_write` force-syncs
  when needed (§3.4). The linear-scan FSM interaction is separate and still
  open; no dedicated large-single-table stress test was added.
- Peak RSS has stayed ~27–28 MB across M0/M1 measurement points.

---

## 12. Known limitations & tech debt registry (consolidated)

The "no vacuum, anywhere" family — all safe (MVCC re-checks make stale
entries invisible), all unbounded space growth:

| Structure | Leak | Since |
|---|---|---|
| Heap tuples | dead versions never reclaimed; pages only grow | M1 |
| Catalog pages | every DDL/RLS change strands the previous blob's page | M1 |
| `VectorIndex` / `InvertedIndex` | old values of UPDATEd rows indexed forever | M2 |
| `EdgeIndex` / `CsrIndex` | aborted/superseded edges never retracted | M3/M7 |
| `__consumers__` | every ack leaves a dead offset version; `vacuum_events` does **not** touch it | M4 |

**Correction (M10, 2026-07-08, branch `core-vacuum`):** the *heap tuples* row
above is now addressed — `Engine::vacuum()` physically reclaims dead heap
versions (reader-aware horizon, crash-safe redo-only `WAL_VACUUM`, page
compaction + slot reuse). Its index-vacuum pass also scrubs the reclaimed
`RowId`s from `VectorIndex`/`InvertedIndex`/`BTreeIndex`/`EdgeIndex`, so those
"indexed forever" / "never retracted" leaks are bounded for a vacuumed table
too — and, more importantly, must be scrubbed *before* a slot is reused, or a
stale entry would alias a live, wrong row (the M10.c aliasing hazard).
`CsrIndex` is deliberately left un-scrubbed (no incremental remove; not
consulted by any read path; rebuilt on open). Still open: this is **manual**
vacuum only (no autovacuum), catalog-page and cross-page/`VACUUM FULL`
reclamation are not done, and index structures shrink only by entry removal,
not physical rebuild — see `docs/backlog/m10_heap_vacuum_gc.md`.

**CSR is not currently consulted by any query path** (added post-M7,
corrected during M8 merge) — it is built, kept warm on every live edge
write, rebuilt on open, and benchmarked in isolation, but `edges_from`/
Cypher always use `EdgeIndex` (see §7.3). A future fix needs a
staleness/generation marker proving CSR has incorporated every write up to
a specific point before it can be safely preferred again.

Performance debt: per-statement fsync — **group commit landed on the server
writer thread 2026-07-08 (branch `m9-group-commit`), read-only-txn commit
fsync fixed, and buffer-pool force-WAL-on-evict landed** (§3.4/§11.1), the
last of which also largely resolves the `BufferPoolFull`-at-scale item
below; the one remaining follow-up is a concurrent read path (readers off
the single writer thread) — see
`docs/backlog/group_commit_and_read_concurrency.md`. WAL truncation rewrites
the whole file (needs log segments); FSM is a linear scan; ~~256-frame
buffer pool + `BufferPoolFull` at scale~~ (largely fixed — see above; FSM
scan cost remains); HNSW full rebuild per upsert; CSR full
rebuild per debounce pass (currently moot — CSR isn't consulted, see
above); `poll_events` full-scan (needs a `seq` index); SSE
poll-per-subscriber.

Functional gaps (deliberate scope, tracked): RC re-evaluation
(EvalPlanQual) unimplemented — `WriteConflict` at all isolation levels;
SSI is a no-op seam; no wait queue/deadlock detection (by design, D12);
catalog DDL not transactional; SQL grammar gaps (no OR/ORDER
BY/LIMIT/aggregates/joins/subqueries/`IN` — parked as Phase 2); no
full-text SQL operator; single-column indexes only; no cost-based index
selection; Cypher is single-hop read-only, nodes are opaque i64s; no CSR
reverse (`to_id`) traversal; RLS is Rust-API-only; manual heap vacuum only
(`Engine::vacuum()`, M10) — no *automatic*/threshold-driven autovacuum.

Server gaps: no multi-request transaction sessions; no TLS (reverse-proxy
assumption); verify-only JWT with no scopes (any valid token can hit
`/checkpoint`); no gRPC; no writer-thread self-healing (process restart is
the recovery model); ~~read routes inherit the read-only fsync~~ (fixed
2026-07-08 — read-only commits no longer fsync). **Concurrent reads (6b,
2026-07-08):** point reads (`GET /rows/:id`) now run off the single writer
thread on a `Send + Sync` `ReadHandle` — a frame-free `SharedPageReader` over
the buffer pool's `Arc<RwLock<PageFileMmap>>` plus the shared `Arc<Mutex>`
txn snapshot state — so a read allocates no xid, writes no WAL, and never
touches the writer's request channel. `Engine` itself stays deliberately
non-`Sync`; `ReadHandle` is the shared-reader type (asserted `Send + Sync`).
Read-only SQL `SELECT` (`POST /sql`) also runs on this path: `Engine.catalog`
is behind an `Arc<RwLock>` (readers need the live `TableDef.pages`), and
`ReadHandle::execute_sql` reuses a `PageReader`-generic `exec_select_readonly`;
`is_concurrent_read_sql` classifies each statement so the handler routes
reads to the read handle and writes/DDL/`NEAR` to the writer. *Writes* still
serialize through the single writer thread (by design — fsync-/group-commit-
bound). `NEAR`/graph/queue reads remain writer-side for now (additive).

---

## 13. Locked decisions index (D1–D13)

| # | Decision | Where it lives / is enforced |
|---|---|---|
| D1 | Steal + no-force (ARIES): redo **and** undo logging | `wal.rs` record format; `recovery.rs` both passes |
| D2 | Per-statement mini-transaction is the M0 atomic unit | `wal.rs` mini-txn bracketing; every `heap.rs` mutation |
| D3 | Control file is the recovery root | `control.rs`; `recovery.rs` starts there; extended (not re-litigated) with `catalog_root` (M1) and `next_xid` (v3, signed off) |
| D4 | Tuple header reserves MVCC bytes up front | `page.rs` 24-byte header; used since M1 with format bump v1→v2 |
| D5 | No dirty page flushes ahead of durable WAL | `bufferpool.rs::flush_page()` + `find_victim()` (steal-point `debug_assert!`, P1.b); + fsync-failure poison (P1.b); crash harness P1–P12 |
| D6 | Single-file storage (WAL separate) | unchanged; revisit was gated post-M4, not yet re-opened |
| D7 | Crash-injection harness, simple by design | `tests/crash/main.rs` P1–P12 (P10 = mid-vacuum M10, P11 = torn-page/`WAL_FPI` P1.a, P12 = fsync-failure poison P1.b) + property test |
| D8 | 8 KiB pages, init-time config, immutable after | `format.rs`; baked into control file |
| D9 | Little-endian, CRC32+LSN per page, magic+version | `format.rs`/`page.rs`/`wal.rs`; `FORMAT_VERSION = 4` (v3→v4 for `WAL_FPI`, P1.a) |
| D10 | RC default, RR available, same snapshots | `txn.rs` snapshot lifetime |
| D11 | `on_read`/`on_write` no-op seam for future SSI | `concurrency_hooks.rs`, threaded through all heap paths |
| D12 | SI abort-on-conflict before RC re-evaluation | `lockmgr.rs` (no wait queue); RC path still pending |
| D13 | Structured logging from day one | `tracing` on WAL/checkpoint/recovery; `/metrics` shipped with M5 |

---

*Document version: covers M0–M8 complete (through commit `af5601b`,
including the M7 CSR-traversal correction found during M8 merge
verification), plus the post-M8 performance track (2026-07-08):
group commit + read-only-fsync-skip + buffer-pool force-WAL-on-evict on
branch `m9-group-commit`, and the concurrent read path (shared
`ReadHandle`) — point reads on branch `m9-concurrent-reads` and read-only SQL
`SELECT` on `m9-concurrent-select` — see §3.3, §3.4, §4.4, §8, §11.1, §12 and
`docs/backlog/group_commit_and_read_concurrency.md`. `NEAR`/graph/queue reads
remain writer-side. **M10 (2026-07-08, branch `core-vacuum`): heap vacuum / MVCC
GC** — `Engine::vacuum()` with a reader-aware horizon, crash-safe redo-only
`WAL_VACUUM`, the secondary-index vacuum gate, and page compaction + slot reuse;
adds crash point P10 and retires the "heap tuples never reclaimed" tech-debt
item (see §12's correction note and `docs/backlog/m10_heap_vacuum_gc.md`).
**Phase 1 — ACID & storage foundation (2026-07-08, branch `acid-hardening`):
P1.a full-page-writes + P1.b fsync-failure handling shipped** — `WAL_FPI`
torn-page protection (§3.3, `FORMAT_VERSION` 3→4, crash point P11) and the
fsyncgate poison path (§3.3, crash point P12); 14 crash tests total. P1.c–P1.e
(`alloc_page` remap + configurable pool + real FSM, isolation correctness incl.
the still-pending D12 RC re-eval + SSI, auto-checkpoint) to follow. See `docs/backlog/phase1_acid_hardening.md` and `PROGRESS.md`'s Phase 1
entry. Update alongside the next checkpoint's closeout.*
**Phase 2 P2.a (2026-07-08, SQL lane, branch `sql-types`): DECIMAL + TIMESTAMP**
— `ColumnType::Decimal(p,s)`/`Timestamp`, row-encoding tags 6/7 (§4.6), exact
fixed-point + UTC-micros representations, working under M11 constraints; see
`docs/backlog/phase2_data_model.md` and `PROGRESS.md`'s P2.a entry.
Update alongside the next milestone's closeout.*
