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
2. **Vector search** — `VECTOR(n)` columns, durable on-disk IVF-Flat index, `NEAR` operator.
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
Query & execution (M1+)       SQL parser -> logical plan -> executor; Cypher subset (M3);
                              joins/aggregates/subqueries/CTEs + cost-based optimizer + EXPLAIN (Phase 4)
Logical record layer (M1+)    rows / vector records / graph edges / queue events
Transaction & concurrency     MVCC snapshots; real lock manager (S/X modes, blocking
                              waits, wait-for-graph deadlock detection); concurrent writers (Phase 5)
Storage layer (M0)            single-file paged store; buffer pool; WAL; control file; recovery
```

Module map (what each layer became in code):

| Layer | Modules |
|---|---|
| Storage core (M0) | `format.rs`, `control.rs`, `mmap.rs`, `page.rs`, `bufferpool.rs`, `wal.rs`, `heap.rs`, `checkpoint.rs`, `recovery.rs` |
| Transactions (M1; concurrent since Phase 5) | `mvcc.rs`, `txn.rs`, `lockmgr.rs`, `concurrency_hooks.rs`, `query_limits.rs` (P5.f timeouts/cancel/`work_mem`) |
| Catalog & SQL (M1) | `catalog.rs`, `sql/{parser,logical,executor}.rs` |
| Query power (Phase 4) | `sql/{query,plan,query_exec,join,aggregate,sort,optimizer,statistics,explain}.rs` — joins (hash+Grace-spill / sort-merge / index-nested-loop), aggregation + sort, subqueries/CTEs, `ANALYZE` + cost-based optimizer, `EXPLAIN` |
| System catalog introspection (Milestone 18) | `sql/information_schema.rs` — `information_schema.*` / `unidb_catalog.*` as synthesized virtual relations SELECTable over the query surface (resolved at plan time in `sql/plan.rs`, rows materialized in `sql/query_exec.rs::Runner::scan`); read-only projection of `catalog.rs` metadata, no storage. See `docs/engine_access_guide.md` |
| Secondary indexes (M2, M6, M7; all durable since Phase 3) | `btree_index.rs` (durable `DiskBTree`, also backs full-text + edge, and the per-table durable **free-space map / page directory** since the durable-FSM milestone), `disk_vector.rs` (durable IVF-Flat `DiskIvfIndex`), `fulltext.rs` (tokenizer), `vector.rs` (retired in-RAM HNSW baseline), `csr_index.rs` (retired) |
| Graph (M3, M7) | `graph/{edges,index,logical,parser,executor}.rs` |
| Event queue (M4) | `queue/{mod,payload}.rs` |
| Server (M5, feature-gated; REST enrichment item 12) | `server/{engine_handle,error,dto,handlers,router,auth,sse,tls,txn_session,cursor}.rs`, `bin/unidb-server.rs` |
| Autovacuum (A1–A4) | `autovacuum.rs` (background `std::thread` launcher: `Weak<Engine>`, threshold policy, clean-shutdown handle) |
| Engine facade | `lib.rs` (`Engine` — the sole entry point) |
| Attach client (M8, separate workspace crate) | `unidb-attach/src/lib.rs` (`AttachClient`, `AttachError`) |

Two invariants shape everything above the storage layer:

- **The engine is synchronous, but `Send + Sync` and safely shared across
  threads (Phase 5).** Every method takes `&self`; all mutable state is behind
  interior-mutable latches/locks/atomics (buffer-pool page latches, the
  `Mutex<WalInner>` WAL, the `&self` lock manager, `Mutex<ControlData>`, …). The
  async worker is retired (Phase 3); the server no longer owns the engine on one
  writer thread — it shares an `Arc<Engine>` across a pool of blocking worker
  threads (P5.e), so writers run in parallel. The engine itself pulls in **no
  async runtime**: a default build has zero async dependencies (verified via
  `cargo tree -p unidb --no-default-features --edges normal` — no
  tokio/reqwest/axum). Durability under concurrency uses a leader-election
  **group-commit** barrier (`Wal::sync_up_to`) whose fsync runs with the append
  lock released, so committers coalesce behind one fsync and write throughput
  scales with cores.
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
- `FORMAT_VERSION` is currently **5**: v1→v2 for M1's tuple-header
  extension, v2→v3 for the `next_xid` control-file field (§4.7), v3→v4 for
  the `WAL_FPI` full-page-image record (P1.a torn-page protection, §3.3), v4→v5
  for the durable B-Tree's `WAL_INDEX` record + `PAGE_TYPE_BTREE` node pages +
  the per-column `index_root` catalog pointer (P3.a, §5.2). No migration paths —
  no earlier version ever shipped externally.
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
  treat as one. The redo/undo bracketing is unchanged; only *when* the log is
  fsynced has moved (see the durability protocol below).
- **Durability protocol: group-committed force-log-at-commit (default since
  2026-07-09).** Statement mini-txns issued **inside an open user transaction**
  append their WAL records *without* a per-statement fsync; `Engine::commit`
  forces the transaction's commit record durable via `Wal::sync_up_to`, the
  single durable point — **one group-coalesced fsync per transaction** (the
  leader's `sync_all` runs with the append lock released, so concurrent
  committers coalesce behind it). This is ARIES' *force-log-at-commit*, which
  **fulfills D1** (per-statement fsync was an over-fulfillment inherited from
  M0, where a statement *was* the transaction); **D2 and D5 are unchanged**.
  Durability is a transaction-granularity promise: no commit is acknowledged
  until its commit LSN is synced; syncing uncommitted statements bought nothing
  (a mid-txn crash rolls back either way). **Standalone operations** that claim
  durability without a following user commit self-sync: checkpoint (`wal.sync()`
  before `flush_all`), vacuum, `set_column_index`, `enable_events`; slot
  metadata and backups fsync their own files (the C1 durability-claim audit,
  `PROGRESS.md`). The legacy **per-statement** policy survives only as an
  internal `#[doc(hidden)]` `set_deferred_sync(false)` so the crash harness can
  exercise both; `synchronous_commit=off`-style ack-before-flush is a genuine D
  violation and is deliberately *not* offered as the default. Was the single
  largest per-commit cost in the engine (see §11); the decomposition ladder put
  the multi-model write tax at ~97% fsync multiplication (`benches/decompose.rs`).
- WAL records use length-prefix framing (`u32` LE) plus a per-record CRC32;
  a recovery scan stops cleanly at the first corrupt/truncated record.
- **D5 — the invariant that must never break**: a dirty page may not be
  flushed or evicted while `page.LSN > durable_WAL_LSN`. Enforced in
  exactly two places: `bufferpool.rs::flush_page()` and the clock
  eviction's `find_victim()` path. This is a tested invariant (debug
  assertions + crash harness), not folklore. **Under deferral (C2), eviction
  that finds no evictable victim — because every dirty frame leads the durable
  WAL — forces `wal.sync()` and retries (`fetch_page_for_write`) rather than
  failing with `BufferPoolFull`**, so a transaction dirtying more pages than the
  pool holds still completes (crash point Pd). Recovery advances the pool's
  durable frontier to the on-disk WAL tail before replaying, so redo may evict
  freely (all replayed records are already durable).
- **Replication shipping is capped at the durable frontier (C3).** Since
  deferral appends records to the segment file before their fsync, `Wal::
  records_from`/`ship_from` ship only records with `lsn <= durable_lsn` — a
  replica can never apply (or, on failover, retain) a commit the primary had
  not made durable, so its state is always a prefix of the primary's durable
  state (see `replica.rs`'s divergence test).
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

**Configurable** capacity (P1.c): `DEFAULT_POOL_CAPACITY = 4096` frames
(32 MiB at 8 KiB pages, raised from the old fixed 256), overridable via the
`UNIDB_BUFFER_POOL_PAGES` env var or `Engine::open_with_pool_capacity`.
Pin/unpin, clock eviction, dirty tracking, D5 enforcement on flush/evict.
`fetch_page` returns a per-call page copy — cheap for point access,
measurably expensive for hot-hub scans, which is what motivated the graph
layer's batch-latch optimization (§7.2). A real scaling limit was
discovered in M6: tables growing into the hundreds of pages could exhaust
the pool (`DbError::BufferPoolFull`) even with small individually-committed
transactions.

**Chunked file growth (P1.c).** `alloc_page` previously `set_len`'d and
**re-mapped the whole file on every allocation** — O(inserts) full-file
remaps, O(N²) total, fatal at 100s of GB. Now the file grows in 4 MiB chunks
(`ensure_mapped` / `grow_chunk_pages`), re-creating the mmap only when a new
page crosses the mapped boundary. `mapped_pages` (physical) is tracked apart
from `file_page_count` (logical high-water mark); `logical_page_count` skips
trailing all-zero slack on open so a reopen reuses it rather than leaking a
chunk. Benchmarked flat at ~1M `alloc_page`/s to 100k pages (`benches/scale.rs`).

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
larger than the pool and largely closes the M6 limit.

**Real free-space map (P1.c).** The heap's `find_or_alloc_page` no longer
scans (and *fetches*) every page to find room — the old O(pages) per-insert
cost that made a heap O(pages²) to fill. `Heap::free_map` caches free bytes
per page, so page selection is integer comparisons; only pages whose free
space is still unknown (a `from_pages`-reconstructed heap) are probed, from
the end backward (append locality), caching each. Benchmarked: insert
throughput does **not** degrade as a heap grows to 300k rows (`benches/
scale.rs`). **Since the durable-FSM milestone (2026-07-10)** a catalog table's
page directory + free-space map is a per-table durable `DiskBTree` keyed
`page_id → free_bytes` (`TableDef.fsm_meta`), not the old in-catalog `pages`
blob: `Heap::open` is O(1) (no directory load), the SQL insert path appends at
the durable tail via `DiskBTree::max_entry` (O(log n), no per-statement rebuild),
and a full scan/vacuum lazily warms the free map from the tree. This closed the
O(heap-pages) catalog-blob `HeapFull` ceiling (see §12 and `docs/backlog/
durable_fsm_catalog_pagelist.md`). The legacy raw-CRUD `self.heap` still uses the
in-memory `free_map` described above.

### 3.5 Checkpoint & recovery

`checkpoint::run`: flush all dirty pages → reset FPI tracking (P1.a) → write a
checkpoint WAL record → persist control file (checkpoint LSN, WAL tail,
`catalog_root`, `next_xid` captured *before* truncation) → truncate the WAL
before the checkpoint LSN (currently by rewriting the whole file — tracked
debt; segmented WAL is Phase 6).

**Auto-checkpoint (P1.e).** Checkpoint was manual-only, so the WAL (and the
P1.a full-page-image volume) grew unbounded. `Engine::maybe_auto_checkpoint`
(called from `commit`) now runs the existing path inline when a **time**
trigger (`checkpoint_timeout`, default 60 s) or a **WAL-size** trigger
(`max_wal_size`, default 64 MiB — `Wal::wal_bytes` is a running counter reset on
truncation) fires — but only at a **quiescent point** (`TransactionManager::
active_count() == 0`), because `checkpoint::run` truncates *all* WAL before the
checkpoint LSN and would otherwise discard an in-flight transaction's undo
records. Config: `AutoCheckpointConfig` (env `UNIDB_AUTO_CHECKPOINT` /
`UNIDB_CHECKPOINT_TIMEOUT_SECS` / `UNIDB_MAX_WAL_SIZE_BYTES`). Benchmarked WAL
bounded (~50–154 KB vs 1.17 MB unbounded) at flat throughput
(`benches/checkpoint.rs`). Reuses the P2/P4-tested checkpoint+recovery path, so
no new crash point. Caveat: a permanently-open long-lived transaction blocks
auto-checkpoint (documented footgun, like Postgres).

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
**physical undo** (§4.3) — *and* that undo must run while the aborting xid is
still in the active set (§4.3, item-16 correction 2026-07-12): the "no aborted
state" shortcut is only sound if a transaction's tuples are physically gone
before other snapshots stop seeing it as active.

### 4.2 Isolation levels (D10, D12; SSI completed P1.d)

- **READ COMMITTED** (default), **REPEATABLE READ** (= snapshot isolation),
  and **SERIALIZABLE** (P1.d). RC vs RR differ *only* in snapshot lifetime
  (per-statement vs per-transaction); SERIALIZABLE uses RR's fixed snapshot
  **plus** SSI rw-antidependency tracking. Same MVCC machinery.
- **Conflict classification (P1.d, completing D12).** A write-write conflict
  under RR/SERIALIZABLE now surfaces as `SerializationFailure` (a real
  serialization anomaly — the caller retries), not a raw `WriteConflict`.
  Under RC the *committed*-superseder case never reaches the conflict at all:
  each statement takes a fresh snapshot, so the scan-based executor re-reads
  the latest committed tip and applies to it (EvalPlanQual is inherent to a
  re-scanning executor — no spurious abort). The only conflict that can fire
  at RC is against a *still-active* writer, which a no-wait engine (D12) must
  reject; blocking-then-EvalPlanQual for that case needs a lock wait queue
  (Phase 5).
- **SSI (P1.d — true SERIALIZABLE).** Cahill-style pivot detection: each
  serializable txn tracks its read and write sets (`txn.rs::SsiState`);
  `ssi_note_reads`/`ssi_note_write` (fed by `exec_select`/`exec_update`/
  `exec_delete`) form rw-antidependency edges between concurrent serializable
  txns; a txn that ends up with both an inbound and an outbound edge (a pivot)
  is aborted at commit with `SerializationFailure`, so write-skew is
  prevented. **Reduced form** (as planned): row-granularity (no predicate
  locks → no phantom protection), statement-granularity tracking at the
  executor, and a write-skew pair may both abort in some orderings (sound,
  occasionally over-conservative).
- **The `on_read()`/`on_write()` seam (D11)** still exists in every heap
  scan/lookup path (`concurrency_hooks.rs`), kept no-op: P1.d's SSI tracks at
  the executor (where the txn context lives) rather than threading a tracker
  through every `heap` method, leaving the seam available for finer-grained
  (e.g. index-level) tracking later.

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
- **Abort ordering (item-16 correction, 2026-07-12 — was a concurrency bug,
  not a tradeoff).** `abort()` must keep the xid in the `active` set (and keep
  its row locks held) for the *entire* physical undo, dropping it and releasing
  locks only afterward. The pre-fix code removed the xid from `active` *first*
  and then undid the heap — opening a window in which, because visibility has no
  "aborted" state (§4.1), a concurrent snapshot classified the still-present
  writes of the aborting txn as *committed*: its doomed UPDATE version became
  visible while the old version it superseded became invisible. A concurrent
  reader then returned a wrong row/count; worse, a concurrent writer could
  acquire the *unlocked* new-version `RowId` and build a fresh chain on top of
  it, after which undo restored the old version — leaving two live versions of
  one logical row (a persistent duplicate) or none (a persistent missing row).
  This was the root cause of backlog item 16's whole symptom family (including
  the intermittent D5-flush error and the >120 s hang, which were downstream of
  the corruption). Proven by a deterministic pin-the-midpoint test
  (`txn.rs::aborting_txn_new_version_never_visible_to_concurrent_snapshot`) and
  a contended geometry regression
  (`tests/concurrent_writers.rs::item16_readers_during_cross_row_churn_*`); see
  `docs/backlog/16_concurrent_sql_writes_visibility_anomaly.md`.

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
(`Int64`/`Text`/`Bool`/`Json`/`Vector(n)`/`Decimal(p,s)`/`Timestamp`/`Float`/
`Uuid`/`Bytea`/`Date`/`Time` — everything past `Vector(n)` added across Phase 2
P2.a–P2.b), persisted as a single
`serde_json` blob rewritten to a fresh page on every change, pointed at by
`control.catalog_root`. Using `serde` here is deliberate — schema is
infrequent control-plane data, not the hot path D9 protects.
`TableDef.fsm_meta: Option<PageId>` holds the stable meta page of each table's
**durable free-space map** (a `DiskBTree` keyed `page_id → free_bytes` whose keys
are the page directory — durable-FSM milestone); the executor reconstructs a
`Heap` handle per statement via `Heap::open` (O(1) — the directory is not loaded
until a full scan needs it). The legacy `TableDef.pages: Vec<PageId>` field is
retained only as a `#[serde(default)]` fallback for pre-FSM catalogs (no
migration). Storing the page list *inline in the catalog blob* was the
O(heap-pages) `HeapFull` ceiling this milestone removed (§12).

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
**Phase 4 lifted the old single-table scope** (this paragraph's earlier "not
supported: `OR`/`ORDER BY`/`LIMIT`/aggregates/joins/subqueries/`IN`" list is
**superseded** — those all ship now). Anything beyond a trivial single-table
filter/project is routed by the parser into a new `LogicalPlan::Query`
(carrying a `QuerySpec`) that the Phase-4 planner turns into a physical
operator tree; the flat single-table `SELECT` path is unchanged (it still
feeds the concurrent-read fast path). Supported: inner/left/right/cross joins,
`COUNT`/`SUM`/`AVG`/`MIN`/`MAX`, `GROUP BY`/`HAVING`, `ORDER BY`/`DISTINCT`/
`LIMIT`/`OFFSET`, scalar/`IN`/`EXISTS` subqueries (correlated + uncorrelated),
`IN (list)`, non-recursive `WITH` CTEs, `ANALYZE`, and `EXPLAIN [ANALYZE]`.
Still out of scope (documented Phase-4 limits): window functions, recursive
CTEs, `FULL OUTER`/`USING`/`NATURAL` joins, and columnar/vectorized execution.

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
filter through the same `predicate_matches`. Originally Rust-API-only
(`set_rls_policy`), since `Expr` has no untrusted serialization design;
REST enrichment R3 (item 12) closed that by adding
`Engine::set_rls_policy_sql` — the policy arrives as a **SQL predicate
string**, parsed by the ordinary parser (plan `SELECT * FROM t WHERE
<pred>`, extract the predicate), so there is still no `Expr` wire format —
and a superuser-gated `PUT /tables/{t}/rls` route over it. There is still
no `CREATE POLICY` SQL statement.

Row encoding is hand-rolled tag+value per column (that *is* the hot path):
tags 0=`Null`, 1=`Int64`, 2=`Text`, 3=`Bool`, 4=`Json`, 5=`Vector`
(`[dim: u32 LE][f32 LE × dim]`, dimension-prefixed so `decode_row` can
cross-check the schema), 6=`Decimal` (`[i128 LE (16 B)][scale: u8]`, P2.a),
7=`Timestamp` (`[i64 LE micros]`, P2.a), 8=`Float` (`[f64 LE]`), 9=`Uuid`
(`[16 B]`), 10=`Bytea` (`[u32 LE len][bytes]`), 11=`Date` (`[i32 LE days]`),
12=`Time` (`[i64 LE micros]`) — the last five from P2.b. New tags are additive
and forward-compatible (D4) — old rows never carry them, so no `FORMAT_VERSION`
bump. Vector dimension (and, for decimals, the stored scale) is validated in
three independent places (DDL, INSERT/UPDATE coercion, every decode), each
guarding a different failure mode.

---

## 5. Secondary indexing framework (M2 + M6; all indexes durable since Phase 3)

> **Phase 3 note (P3.a + P3.b + P3.c — Phase 3 complete):** **every** secondary
> index is now **durable, synchronous, WAL-logged, and crash-recovered — never
> rebuilt on open.** The B-Tree (P3.a), full-text/inverted (P3.b), and
> edge-adjacency (P3.b) indexes are on-disk B+trees (`DiskBTree`); the vector
> index (P3.c) is an on-disk IVF-Flat (`DiskIvfIndex`) whose cell posting lists
> are themselves a `DiskBTree` and whose centroids live in a WAL-logged meta
> page. All are updated inline on the write path
> (`apply_durable_index_writes`, `graph/edges::ensure_edge_index`) and read from
> their stable meta pages. The graph/LOB write paths are serialized by
> `Engine::write_serial`. The SQL write path was, through Phase 5, serialized by
> the catalog `RwLock`; the **index-write-concurrency** milestone (the
> `UNIDB_CONCURRENT_SQL_WRITES` toggle, **default-ON as of the item-11 flip
> 2026-07-13** — set it to `0`/`false`/`off` for the serialized fallback) lets
> catalog-non-mutating SQL DML take a
> *shared* catalog lock and made `DiskBTree` writes safe under concurrent writers
> via **latch-coupled ("crabbing") descent with safe-node early release** — so
> two writers now maintain the same index in parallel (§5.4). The M7 **CSR index was retired** in P3.b; the
> **async index worker was retired entirely** in P3.c (its last user was the
> in-RAM HNSW). So §5.1 below is historical — there is no background index
> thread anymore. See §5.2 and §5.4.

### 5.1 The async index worker (RETIRED in P3.c — historical)

Through P3.b the engine had a background index thread (`index_worker.rs`) that
owned an `Arc<RwLock<HashMap<(table, column), IndexEntry>>>` built from channel
messages and never received a `BufferPool`/`Wal`/`Heap` handle. It rebuilt the
in-RAM vector index on open and applied live upserts off the write path.
**P3.c made the vector index durable (`DiskIvfIndex`), removing the worker's last
user, so the module was deleted.** `Engine::open` now does **zero index
rebuilding** for every index kind — it reads each index from its stable meta
page. `IndexStatus` (moved to `catalog.rs`) is retained for the REST status
route; a durable index is always `Ready` (computed from the catalog). `CREATE
INDEX` validates kind-vs-column-type, persists via `Catalog::set_column_index`,
and builds the durable index synchronously from committed rows, recording its
meta page id in `ColumnDef.index_root`.

### 5.2 Index kinds

| Kind | Structure | Notes |
|---|---|---|
| `Hnsw` (M2; **durable IVF-Flat since P3.c**) | `disk_vector.rs`, `DiskIvfIndex` — on-disk IVF-Flat; cell posting lists = durable `DiskBTree`, centroids in a WAL-logged meta page | **Durable, WAL-logged, crash-recovered, not rebuilt on open.** The `Hnsw` keyword now *denotes* the IVF-Flat index (the in-RAM HNSW graph in `vector.rs` is retired — kept only as a benchmark baseline). `CREATE INDEX ... USING HNSW`/`IVF` trains centroids from committed rows (`nlist ≈ √rows`, recall-favoring `nprobe`), stored in the meta page (id in `ColumnDef.index_root`). `NEAR` probes the nearest cells' posting lists → exact re-rank from the heap → MVCC re-check. f32, Euclidean (pgvector `<->` convention). |
| `FullText` (M2; **durable since P3.b**) | durable `DiskBTree` keyed on tokens (`fulltext::tokenize`), one `(token, RowId)` entry per token | **Durable, WAL-logged, not rebuilt on open** — same `DiskBTree`/`WAL_INDEX` machinery as `BTree`. Now has a real read path: `Engine::search_fulltext` (tokenize → intersect per-token `search_eq` posting lists, AND-only → MVCC-resolve). |
| `BTree` (M6; **durable since P3.a**) | `btree_index.rs`, `DiskBTree` — on-disk B+tree of buffer-pool-managed pages | **Durable, WAL-logged, crash-recovered, not rebuilt on open** (§5.4). Node pages carry the standard page header + a body-tag; a stable meta page (id in `ColumnDef.index_root`) points at the root so a root split never rewrites the catalog. Mutations log full node-page images (`WAL_INDEX`, redo-only) in one mini-txn each. No undo (entries are MVCC-validated hints). Written synchronously on the write path — **not** via the async worker. |
| `Csr` (M7) | `csr_index.rs` | **Retired in P3.b** — consulted by no read path after the M7 traversal revert (§7.3); adjacency is now served by the durable edge index. The module + its benchmark remain but are unwired from the runtime. |

### 5.3 Query execution against indexes

Both index-assisted paths use the same **resolve-then-refilter template**:
index → candidate `RowId`s → `Heap::get` under the caller's snapshot
(drops `NoVisibleVersion`) → full `predicate_matches` (so remaining AND
terms + RLS apply identically to a full scan).

- **`NEAR` (M2.d)**: over-fetch `max(4k, k+20)` candidates, then filter.
  Requires an HNSW index (clear `SqlPlan` error otherwise — no silent
  full-scan fallback, because approximate top-k has no correct fallback).
  Served by the durable IVF-Flat index (`DiskIvfIndex`, P3.c) — no `Ready`
  status any more (the index is always crash-consistent with committed data); a
  column flagged `Hnsw` but never built (no `index_root`) yields zero candidates.
- **B-Tree (M6.b; durable since P3.a)**: `find_indexable_btree_predicate`
  detects a top-level or AND'd `Column <op> Literal` on a BTree-indexed column,
  then `try_exec_select_btree` reconstructs the `DiskBTree` from the column's
  `index_root` and calls `DiskBTree::search`. **No `Ready` status any more** —
  the durable tree is always crash-consistent with committed data, so there is
  no backfill window; a column flagged BTree but never built (no `index_root`)
  simply falls back to a full scan. First indexable term wins; there is no
  cost-based selection (§12).

### 5.4 Durable B-Tree (P3.a)

`DiskBTree` (`btree_index.rs`) is the first durable secondary index and the
Phase-3 template. Nodes are pages in the shared page store carrying the standard
28-byte header (so the buffer pool's CRC + D5 discipline applies unchanged); a
body tag distinguishes meta / internal / leaf, leaves are right-linked for range
+ duplicate walks. Every `insert`/`remove` is **one WAL mini-transaction**
bracketing each page it touches (a leaf write, or a split-chain + root-repoint +
meta-page update), logged as full node-page images (`WAL_INDEX`, redo-only,
`slot == u16::MAX`); recovery redoes all pages of a committed index mini-txn or
none. **There is no undo** — an index entry is a hint re-validated against MVCC
visibility, so a stale/extra entry is harmless, and the only dangerous case (a
committed visible row lacking its entry) is prevented by the index mini-txn
fsyncing before the user txn's `WAL_TXN_COMMIT`. `Engine::open` reads the tree
from its meta page in O(1) — the durability win benchmarked in `PROGRESS.md`'s
P3.a entry. Vacuum scrubs it directly (`DiskBTree::remove`, reading each dead
row's key via `Heap::get_raw` before the slot is reused). v1 leaves underfull
nodes un-merged (tree only grows) and pays one fsync per key insert.

**Concurrent writers (index-write-concurrency milestone).** Two writer threads
can now insert into the *same* tree at once (before, the SQL catalog write lock
serialized them). `insert_in_txn` descends with **latch coupling ("crabbing")**
over the buffer pool's per-page exclusive latches: it latches each child before
releasing the parent, and drops all ancestor + meta latches at the first *safe*
node (one that cannot overflow on a single-entry insert, so it will not split —
`node_is_insert_safe`, exact for `Int`/`Bool` keys, conservative for `Text`).
The still-modifiable path suffix stays latched, a split propagates up through it,
and only a root split repoints the meta page (root never released ⇒ meta still
held). Latches are taken strictly root→leaf, so inserts cannot deadlock.
`set_value`/`remove` re-read the target leaf *under* its exclusive latch (never
writing back bytes read before latching, which a concurrent split could have
superseded). Reads stay latch-free (owned per-page copies + right-linked leaves +
MVCC re-validation make a transiently stale read self-correcting). The protocol
is validated by a structural validator (`DiskBTree::validate`), deterministic
split-contention + concurrent-stress tests, and a `loom` model of the latch
ordering (`loom-crabbing` crate). It is gated behind the
`UNIDB_CONCURRENT_SQL_WRITES` toggle (**default-ON since the item-11 flip
2026-07-13**): set the toggle to `0`/`false`/`off` ⇒ the catalog write lock
serializes SQL writers exactly as before (crabbing latches are uncontended,
behavior unchanged) — the residual-race revert path, one env var, no code revert.
Follow-up: optimistic *shared*-latch descent + a Lehman-Yao B-link (right-linked
internal nodes, format-bump-gated) would let even same-subtree descents overlap.
See `PROGRESS.md`'s "Index & heap write concurrency" entry for the acceptance
numbers.

### 5.5 Durable vector index — on-disk IVF-Flat (P3.c)

`DiskIvfIndex` (`disk_vector.rs`) is the durable vector index, chosen (over
DiskANN/Vamana) because its only on-disk state is a cell posting list
`cell_id → [RowId]`, which *is* a `DiskBTree` (§5.4). It is a stateless handle
over a **stable meta page** (id in `ColumnDef.index_root`, exactly like
`DiskBTree`): the meta page (an `IVF_META_MAGIC` body on a `PAGE_TYPE_BTREE`
page) records metric/dim/nlist/nprobe + the postings tree's meta page + the head
of a **WAL-logged centroid page chain**. Every operation reloads the bounded
(`O(nlist·dim)`) centroid table from the buffer pool, so **centroids are
crash-recovered, never recomputed**, and open is O(1). All pages use `WAL_INDEX`
full-page images — recovered identically to `DiskBTree` nodes, **no new record
kind / page type / `FORMAT_VERSION` bump**.

- **Build** (`CREATE INDEX ... USING HNSW`/`IVF`): train `nlist ≈ √rows` (capped
  256) centroids from the committed rows via mini-batch Lloyd's k-means, persist
  meta + centroids, insert each `(cell, RowId)` into the postings tree. An
  empty-table create trains one origin cell (correct-but-flat until re-created).
- **Search** (`NEAR`): rank centroids by distance, probe the `nprobe` nearest
  cells' posting lists (recall-favoring default), fetch candidate vectors from the
  heap for an **exact re-rank** (IVF-Flat has no quantization error), then the
  same MVCC/RLS/predicate re-check as every other index path.
- **Maintenance**: `apply_durable_index_writes` inserts on INSERT/UPDATE; vacuum's
  aliasing gate scrubs it via `DiskIvfIndex::remove` before a reclaimed slot is
  reused (re-deriving the cell from the dead row's vector).

Recall@10 = 1.000 matches the retired in-RAM HNSW baseline at bounded RAM
(`benches/vector_recall.rs`); crash point **P17** proves recovery with recall
intact. Re-training as a maintenance op (so a table that grew after an empty-table
create re-clusters) is a documented follow-up.

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

### 7.2 Read path: durable edge index + batch-latch resolution

The edge index (by `from_id`) became a **durable `DiskBTree`** over
`__edges__.from_id` in P3.b (was an in-memory `HashMap` rebuilt on open).
`create_edge`/`delete_edge` maintain it synchronously and WAL-logged via
`DiskBTree::insert`/`remove(OrderedValue::Int(from_id), rid)`; `edges_from`
and the Cypher executor read it via `search_eq`. Its meta page is cached on
the `Engine` (`edge_index_meta`), created/loaded once by
`graph::edges::ensure_edge_index` at open — **not rebuilt**. It still has no
abort-time cleanup; stale entries are permanently safe because every candidate
is re-validated against the caller's snapshot (§10).

`resolve_candidates_batched` groups candidate `RowId`s by `page_id` so a
hot hub costs one `fetch_page` per page instead of one per edge (~128
edges/page): measured ~9.3–9.7x over naive resolution, and it closes
almost the whole read-side gap with Postgres (94.3 µs vs 98 µs at 1k
edges; 930 µs vs 568 µs at 10k).

### 7.3 CSR index (M7 — retired in P3.b)

> **Retired (P3.b):** the CSR index below was consulted by no read path after
> the M8-era correction described here, and P3.b's durable edge index (§7.2)
> now serves adjacency durably — so its rebuild-on-open + warm-keeping were
> removed. `csr_index.rs` and its `benches/graph.rs` measurement remain but are
> unwired from the runtime. The history below is retained because it is the
> reason CSR was never trusted for traversal in the first place.

A read-optimized adjacency structure built asynchronously on the existing
index worker, sitting **alongside — never replacing —** the synchronous
edge index. `CsrIndex` splits `stage()` (append to a raw `Vec`) from
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

**The core decision (as evolved by P5.e-3): async handlers never run
`Engine` calls on the async runtime.** The original M5 design gave one
dedicated OS thread sole ownership of the `Engine` (handlers sent typed
requests over an mpsc channel); once `Engine` became `Send + Sync`
(P5.e-2), `server/engine_handle.rs` was rewritten to hold one shared
`Arc<Engine>` and run each blocking call on a tokio blocking-pool thread
via `spawn_blocking` — many requests execute concurrently, coordinating
through the engine's internal latches/locks, with group commit coalescing
their fsyncs. `EngineHandle::spawn` still opens the `Engine` synchronously
on the caller's thread so open failures surface as an immediate `Err`.

Surface (see `docs/REST_API.md` for contracts): `POST /sql` (atomic
`;`-separated multi-statement transactions — free, since `execute_sql`
already runs a whole string under one xid; optional one-shot `isolation`
field, R2; optional `cursor` mode, R4), `POST /cypher`, raw row CRUD
(`/rows...`, plus atomic base64 `POST /rows/batch`, R4), graph routes
(`/edges...`), indexing (`POST /indexes`, status), schema introspection
(`GET /tables` — user tables + columns, internal `__…__` tables hidden, S1),
events (`POST /tables/{t}/events`, `GET /events/subscribe` SSE,
`POST /events/ack`, `POST /events/vacuum` R3), RLS
(`PUT /tables/{t}/rls`, SQL-predicate-string policy, superuser-gated, R3),
`POST /checkpoint`, `POST /admin/flush` (superuser-gated, R3),
`GET /metrics` (Prometheus), `GET /stats`, replication routes (P6.b), and
**multi-request transaction sessions** (REST enrichment R1, item 12):
`POST /txn/begin` opens a real client-held transaction whose id later
requests carry via `X-Txn-Id` (no auto-commit), finished by
`POST /txn/{id}/commit|rollback`; the session registry
(`server/txn_session.rs`) serializes in-session requests (`409 TXN_BUSY`),
binds each session to its JWT principal (`403`), and a `Weak`-ref
background reaper auto-aborts idle sessions so an abandoned client cannot
pin the vacuum horizon. Result cursors (`server/cursor.rs`, R4) page large
`rows` results with the same principal-binding + idle expiry. Auth is
**verify-only JWT** (HS256 Bearer; the server never issues tokens);
per-user privileges + superuser gates are P6.e. SSE is
**server-polls-then-pushes** (`poll_events` has no wake primitive), which
is why subscriber cost scales badly (§11). TLS is native since P6.f
(rustls, `UNIDB_TLS_CERT`/`KEY`).

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
explicit `begin`/op/`commit` shape. Every mutating REST route does its own
internal begin→execute→commit when called one-shot (§8), and
multi-statement atomicity is available via `;`-separated SQL passed to
`execute_sql`. Since REST enrichment R1 (item 12) the **server** does
support multi-request transaction sessions (`X-Txn-Id`), but the attach
client deliberately stays one-shot — session support is an optional
follow-up (the wire surface is just a header on the existing calls). This
is a documented API-shape difference from embedded `Engine`, not an
oversight.

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

1. **Secondary indexes are non-transactional derived state whose entries
   are hints, re-validated against MVCC.** Since Phase 3 **every** index kind is
   durable and WAL-logged — B-Tree/full-text/edge as `DiskBTree` (P3.a/P3.b), the
   vector index as `DiskIvfIndex` (P3.c) — yet all still have *no undo*: their
   entries remain hints, not transactional state (a redo-only `WAL_INDEX` entry
   from an aborted txn is a harmless stale hint). What every kind shares is the
   load-bearing invariant: *every candidate an index produces is re-checked
   against the caller's MVCC snapshot (and full
   predicate) before becoming a result.* Stale or phantom entries are therefore
   space leaks, never wrong answers — whether they came from an aborted async
   upsert or an aborted durable-B-Tree mini-txn. Each milestone lands a
   dedicated proof-of-abort-invisibility test (`vector_mvcc`, `graph_mvcc` ×2,
   `btree_mvcc`, `queue_mvcc`) written *next to* the feature, not deferred —
   because the failure mode (a forgotten `record_undo`, a worker with no
   transaction concept) is silent. The one danger unique to a durable index — a
   committed visible row lacking its entry (a false negative, not a false
   positive that rule 1 catches) — is closed for the B-Tree by ordering: the
   index mini-txn fsyncs before the user txn commits (§5.4).
2. **New durable state is always ordinary heap rows** (`__edges__`,
   `__events__`, `__consumers__`) — inheriting MVCC, locking, WAL, crash
   recovery, and abort handling with zero new mechanism. This is why M3–M5
   added no new crash-injection points: there was no new durability
   machinery to crash.
3. **Completeness-sensitive vs approximate readers must gate on index
   readiness *and* on how current the index can ever promise to be.**
   Since P3.a the durable B-Tree (exact results) needs no such gate — it is
   always crash-consistent with committed data, so there is no `Building`
   window; a column with no built tree simply falls back to a full scan. `NEAR`
   (approximate top-k) still accepts partial results while `Building` — correct
   because its contract already permits "may return fewer results while not yet
   caught up." **CSR (graph) was originally given
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
"replaced stack" benchmark **shipped in item 17** (was a deferred follow-up):
`benches/decompose.rs` Table 4 under `MM_REPLACED_STACK=1` runs the same
four-model write as four independent PG commits (row + pgvector+HNSW + a graph
adjacency table + an outbox queue, no shared transaction) — **unidb's one atomic
commit is 3.61× faster under real flush-to-platter durability** (`F_FULLFSYNC` vs
`fsync_writethrough`; 250 vs 69 txns/s = 1 sync vs 4), narrowing to ~parity under
a cheap/buffered VM `fsync` (the win is proportional to real durable-sync cost).
The **unconditional** win is crash-consistency: unidb recovers 0 orphans where the
stack recovers a torn record (`tests/crash` `item16_*`, harness 29 → 31). Real
polyglot infra (Neo4j/Kafka/Qdrant) stays a heavier follow-up.

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
  **Correction (2026-07-09, commit-time WAL fsync):** this per-statement cost
  is *gone by default* — statement mini-txns inside a user transaction no
  longer fsync; only `Engine::commit`'s `sync_up_to` does (§3.3). Batching now
  amortizes to **one fsync per transaction**. The decomposition ladder
  (`benches/decompose.rs`) measures the full multi-model commit (row + B-tree +
  vector + edge + event) dropping from ~33.1 ms to ~4.3 ms/commit — **~7.7×** —
  at one fsync per commit, and W0 (plain row) at SQLite parity.

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

- **~~HNSW rebuild-per-upsert~~ (resolved in P3.c)**: the in-RAM HNSW's
  full-graph-rebuild-per-upsert (no incremental insert in `instant-distance`) is
  gone — the vector index is now the durable IVF-Flat `DiskIvfIndex`, whose
  per-insert cost is a single posting-list `DiskBTree` insert (O(log n)) and whose
  open cost is O(1). CSR (also non-incremental, debounced) is likewise retired.
- **SSE subscriber scaling**: 1→10→50 subscribers is 5.2→33.9→162.6 ms —
  N pollers × poll interval × linear `poll_events`, all serialized
  through the one writer thread.
- **`NEAR` latency (~4–5 ms)** is transactional overhead, not vector
  search — the raw structures answer in microseconds (fulltext search
  ~14.2 µs).
- **`BufferPoolFull` at ~100k rows/table** (M6 discovery): **fixed** — the
  M9 change made `find_victim` write back + evict dirty pages once durable
  (root cause: its D5 hint was hardwired to `INVALID_LSN`), and **P1.c**
  raised the default pool to 4096 frames (configurable) and replaced the
  O(N²) whole-file-remap-per-`alloc_page` with chunked growth, so heaps scale
  to 300k+ rows with flat throughput (`benches/scale.rs`, §3.4).
- **Linear-scan FSM** (was open): **fixed by P1.c** — `Heap::free_map` makes
  page selection integer comparisons, not a fetch of every page. ~~Remaining
  caveat: the map is per-`Heap`-instance and the SQL path rebuilds a heap per
  statement, so a durable on-disk FSM fork is a later item.~~ **Fixed by the
  durable-FSM milestone (2026-07-10):** each catalog table's page directory +
  free-space map is a durable per-table `DiskBTree` (`TableDef.fsm_meta`), so the
  SQL path no longer rebuilds per statement and the O(heap-pages) catalog-blob
  `HeapFull` ceiling is gone (see §12, `docs/backlog/durable_fsm_catalog_pagelist.md`).
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
consulted by any read path; rebuilt on open). Vacuum is now **auto-triggered**
by a background launcher (Autovacuum A1–A4, branch `autovacuum`): a
`std::thread` holding `Weak<Engine>` wakes every `naptime`, and when
`dead > threshold + scale_factor · live` (Postgres-shape policy over global
dead/live estimates) fires the same `Engine::vacuum` pass — safe without new
locking (`Engine` is `Send + Sync`, vacuum already takes `write_serial` +
per-page latches, horizon is reader/slot-correct). Still open: per-table
accounting + `vacuum_table` + a cost-based throttle, catalog-page and
cross-page/`VACUUM FULL` reclamation, and physical index rebuild — see
`docs/backlog/autovacuum.md` and `docs/backlog/m10_heap_vacuum_gc.md`.

**CSR is not currently consulted by any query path** (added post-M7,
corrected during M8 merge) — it is built, kept warm on every live edge
write, rebuilt on open, and benchmarked in isolation, but `edges_from`/
Cypher always use `EdgeIndex` (see §7.3). A future fix needs a
staleness/generation marker proving CSR has incorporated every write up to
a specific point before it can be safely preferred again.

Performance debt: ~~per-statement fsync~~ **fixed 2026-07-09 (commit-time WAL
fsync): group-committed force-log-at-commit is now the engine default on every
path (§3.3) — statement mini-txns inside a user transaction defer, `Engine::
commit` issues the single coalesced fsync; the eviction-forced-sync path (C2)
keeps it safe under memory pressure and the shipping cap (C3) keeps replicas a
prefix.** (Group commit had already landed on the server writer thread
2026-07-08, branch `m9-group-commit`, with read-only-txn commit fsync fixed and
buffer-pool force-WAL-on-evict; this flips the default for the embedded path
too.) The remaining smoother is a background WAL-writer thread for
eviction-forced syncs under memory pressure (noted, not required for
correctness). WAL truncation rewrites
the whole file (needs log segments); ~~FSM is a linear scan~~ (**fixed P1.c**:
`Heap::free_map`) ~~/ per-`Heap` map rebuilt per SQL statement + an O(heap-pages)
catalog page-list blob that overflowed a page at ~1,450 pages (`HeapFull{8138}`,
capping SQL bulk loads at ~145k rows)~~ (**fixed by the durable-FSM milestone
2026-07-10**: per-table durable `DiskBTree` FSM keyed `page_id → free_bytes`,
`TableDef.fsm_meta`, O(1) open, no per-statement rebuild — `docs/backlog/
durable_fsm_catalog_pagelist.md`); ~~256-frame buffer pool + `BufferPoolFull` at scale~~
(**fixed** — configurable 4096-frame default + chunked file growth, P1.c);
~~`alloc_page` remaps the whole file per page~~ (**fixed P1.c**: chunked
growth); ~~HNSW full rebuild per upsert~~ (**fixed P3.c**: durable IVF-Flat, O(1)
open); ~~CSR full rebuild per debounce pass~~ (retired in P3.b); `poll_events`
full-scan (needs a `seq` index); SSE poll-per-subscriber; ~~UPDATE re-indexed
every row with a full-page `WAL_INDEX` image *per row*~~ (**fixed by
crud-perf Phase A, 2026-07-10**: `exec_update` now accumulates every touched
row's B-tree entries and flushes them **coalesced** via `DiskBTree::insert_many`
— one leaf image per statement, not per row — dropping index-maintenance WAL
from ~8868 to ~619 B/row, UPDATE-bulk 0.11× → 0.34× vs Postgres; a selectivity-
gated `index_matching_rows` also drives *selective* UPDATE/DELETE off the B-tree
instead of a full scan). **Remaining write-path debt (the path to UPDATE
*parity*, deferred):** UPDATE still pays the insert-new-version MVCC cost (a new
heap version + xmax stamp + a fresh index entry per row, ~619 B/row WAL) where
Postgres uses **HOT** (in-place, same page, no index touch) — closing it needs a
forward-chained heap / RowId-preserving update (**A2**, fiddly against the MVCC
model), and DELETE/scan cost needs decode-pushdown + parallelism (**Phase B**).
See `docs/backlog/crud_performance.md` and `PROGRESS.md`'s Phase A entry.
**Read path — crud-perf Phase B (2026-07-10):** ~~the executor decoded the whole
row (every column incl. the `TEXT body` `String`) for every scanned row~~
(**fixed**: `deform_row` materializes only the predicate/projection columns and
stops after the last needed attribute — the PG `heap_deform_tuple` `natts` limit —
wired as two-phase decode into `exec_select`/`matching_rows`/`try_exec_select_btree`;
SELECT-filtered `dec/row 2.00 → 0.00`, `cols/row 8.00 → 5.00`); ~~`SELECT COUNT(*)`
decoded every row into `Literal`s it discarded~~ (**fixed**: `Heap::count_visible`
counts Live+visible slots via headers only — **now 2.81× faster than Postgres**).
~~**Scan-throughput gap** (`SELECT-all`, filtered SELECT at scale) is Postgres's
parallelism~~ (**addressed by Milestone P, 2026-07-10 — parallel scan workers**):
a table's pages are partitioned across `std::thread::scope` workers (not tokio,
§4) each reading the shared mmap. The **pool/mmap read-consistency "landmine" I
flagged does not exist** — unidb is mmap-as-storage (`Frame` = eviction metadata
only; `write_page` writes into the mmap; `read_page` returns an owned copy under
the read-lock), so a worker always sees committed data. Result: unfiltered
`SELECT COUNT(*)` **3.82× faster** in parallel, filtered `COUNT(*) WHERE …`
**6.6× faster** via **partial aggregate** (the whole scan→filter→count runs in the
workers via `parallel_count_matching` + a `QExpr::has_subquery` gate; Postgres's
lead +540% → +82%), and filtered `SELECT … WHERE k …` **6.41× faster** via
`parallel_resolve_candidates` (partitioning the B-tree index-candidate `RowId`
list — `try_exec_select_btree` was the worst ÷PG at ~0.14×). Read-only, so the
crash harness is unchanged. **Worker governance + default-on (item 15,
2026-07-11):** parallel scan originally shipped default-*off* pending governance;
item 15 added a process-wide worker budget (`WorkerLease` RAII admission — total
live workers never exceed `UNIDB_PARALLEL_MAX_TOTAL_WORKERS`, extra queries
degrade to serial rather than oversubscribing M×N threads) plus
timeout/cancellation propagation (`query_limits::snapshot_deadline()`, checked
every few pages → `QueryTimeout`/`QueryCancelled`), then flipped it **default-on**
(`UNIDB_PARALLEL_SCAN=0` / `set_parallel_scan(false)` remain the field revert).
This also fixed "`report.sh` shows no parallel improvement": the bench never set
the toggle, so it ran serial — default-on it now reports the parallel numbers
(Table 3.1 @1M ~5.6M → ~35.7M rec/s). **Remaining read-path debt (deferred):** `SUM`/`GROUP
BY` partial aggregate + `LIMIT` early-stop (only `COUNT(*)` is pushed into workers
so far);
`query_exec` scan projection
needs planner column pruning; a **visibility map** / index-only scan is the true
COUNT accelerator at large scale; `ORDER BY…LIMIT` early-stop and streaming (B3)
are filed. See `PROGRESS.md`'s Phase B + Milestone P entries.

Functional gaps (deliberate scope, tracked): ~~RC re-evaluation
(EvalPlanQual) unimplemented~~ and ~~SSI is a no-op seam~~ (**both fixed P1.d**
— RR/SERIALIZABLE conflicts are now `SerializationFailure`, RC re-reads via its
fresh snapshot, and SSI pivot detection prevents write-skew; reduced form:
row-granularity, no phantom protection); no wait queue/deadlock detection (by
design, D12 — blocking-then-EvalPlanQual for an *active*-writer conflict is
Phase 5); catalog DDL not transactional; SQL grammar gaps (no OR/ORDER
BY/LIMIT/aggregates/joins/subqueries/`IN` — parked as Phase 2); no
full-text SQL operator; single-column indexes only; no cost-based index
selection; Cypher is single-hop read-only, nodes are opaque i64s; no CSR
reverse (`to_id`) traversal; RLS is Rust-API-only; ~~manual heap vacuum only
(`Engine::vacuum()`, M10) — no *automatic*/threshold-driven autovacuum~~
(**fixed by Autovacuum A1–A4** — a background `std::thread` launcher
threshold-triggers `Engine::vacuum`; per-table granularity + a cost throttle
remain future work, `docs/backlog/autovacuum.md`).

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
is behind an `Arc<RwLock>` (readers need the live `TableDef.fsm_meta`, from which
the concurrent-read scan reconstructs the page directory via the FSM tree over
its `SharedPageReader` mmap — `DiskBTree::page_directory` is `PageReader`-generic
for exactly this), and `ReadHandle::execute_sql` reuses a `PageReader`-generic
`exec_select_readonly`;
`is_concurrent_read_sql` classifies each statement so the handler routes
reads to the read handle and writes/DDL/`NEAR` to the writer. *Writes* still
serialize through the single writer thread (by design — fsync-/group-commit-
bound). `NEAR`/graph/queue reads remain writer-side for now (additive).

---

## 13. Locked decisions index (D1–D13)

| # | Decision | Where it lives / is enforced |
|---|---|---|
| D1 | Steal + no-force (ARIES): redo **and** undo logging | `wal.rs` record format; `recovery.rs` both passes; **fulfilled by group-committed force-log-at-commit — the ARIES durability point — as the default (2026-07-09, §3.3)** |
| D2 | Per-statement mini-transaction is the M0 atomic unit | `wal.rs` mini-txn bracketing; every `heap.rs` mutation; bracketing unchanged, only fsync timing moved to commit (§3.3) |
| D3 | Control file is the recovery root | `control.rs`; `recovery.rs` starts there; extended (not re-litigated) with `catalog_root` (M1) and `next_xid` (v3, signed off) |
| D4 | Tuple header reserves MVCC bytes up front | `page.rs` 24-byte header; used since M1 with format bump v1→v2 |
| D5 | No dirty page flushes ahead of durable WAL | `bufferpool.rs::flush_page()` + `find_victim()` (steal-point `debug_assert!`, P1.b); + fsync-failure poison (P1.b); + eviction-forced sync under deferral (C2); crash harness P1–P19 + Pa–Pd |
| D6 | Single-file storage (WAL separate) | unchanged; revisit was gated post-M4, not yet re-opened |
| D7 | Crash-injection harness, simple by design | `tests/crash/main.rs` P1–P12 (P10 = mid-vacuum M10, P11 = torn-page/`WAL_FPI` P1.a, P12 = fsync-failure poison P1.b) + property test |
| D8 | 8 KiB pages, init-time config, immutable after | `format.rs`; baked into control file |
| D9 | Little-endian, CRC32+LSN per page, magic+version | `format.rs`/`page.rs`/`wal.rs`; `FORMAT_VERSION = 4` (v3→v4 for `WAL_FPI`, P1.a) |
| D10 | RC default, RR available, same snapshots | `txn.rs` snapshot lifetime |
| D11 | `on_read`/`on_write` seam; SSI is an addition | seam in `concurrency_hooks.rs`; SSI landed at the executor + `txn.rs::SsiState` (P1.d) |
| D12 | SI abort-on-conflict; RC re-eval; SSI | `lockmgr.rs` (no wait queue); RC re-reads via fresh snapshot + RR/SER `SerializationFailure` + SSI pivot abort (P1.d) |
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
fsyncgate poison path (§3.3, crash point P12); 14 crash tests total. **P1.c
scaling foundation shipped** — chunked `alloc_page` growth, a configurable
buffer pool (4096-frame default), and a real free-space map (§3.4,
`benches/scale.rs`; no crash point — no new durability mechanism). **P1.d
isolation correctness shipped** — RC re-evaluation (via fresh-snapshot
re-scan), RR/SERIALIZABLE write-write conflicts as `SerializationFailure`, and
true SERIALIZABLE via SSI pivot detection preventing write-skew (§4.2; no crash
point — an SSI abort is an ordinary rollback). **P1.e auto-checkpoint shipped**
— time + WAL-size triggers run the existing checkpoint path inline at a
quiescent point, bounding WAL growth (§3.5, `benches/checkpoint.rs`; no crash
point — reuses the P2/P4 checkpoint path). **Phase 1 is COMPLETE — all five
checkpoints (P1.a–P1.e) shipped; the feature-freeze gate is closed** (crash
harness 11→14, `FORMAT_VERSION` 3→4, no locked decision reversed).
**Phase 2 complete (2026-07-08, SQL lane, branch `sql-types`): real data model.**
P2.a DECIMAL+TIMESTAMP · P2.b FLOAT/UUID/BYTEA/DATE/TIME (row-encoding tags
6–12, §4.6) · P2.c ALTER/DROP/TRUNCATE with a **tombstone `DROP COLUMN`**
(`ColumnDef.dropped`) and **request-level DDL rollback** (`execute_sql`
snapshots/restores the catalog root; full crash-safe user-txn-scoped catalog
undo through recovery is a deferred Core-lane follow-up) · P2.d **SERIAL**
(durable `TableDef.serial_next` counter) · P2.e **prepared statements + `$n`
bind parameters** (`Literal::Param` + `bind_params`, `Engine::execute_sql_params`
/`prepare`/`execute_prepared`, `POST /sql` `params`) — closes the SQL-injection
surface. All additive/forward-compatible; no `FORMAT_VERSION` bump.
**Phase 3 COMPLETE (2026-07-09, Core lane): multi-model durable storage — the
moat.** P3.a **durable paged WAL-logged B-Tree** (`DiskBTree`, §5.2/§5.4):
crash-recovered, **not rebuilt on open**; `FORMAT_VERSION` **4→5**. P3.b **durable
full-text + edge index; CSR retired** (§5.2, §7.2): both reuse the
`DiskBTree`/`WAL_INDEX` machinery (no format bump), new `Engine::search_fulltext`
read path. P3.c **durable on-disk vector index** (`DiskIvfIndex`, §5.5, branch
`p3c-vector-production`): on-disk IVF-Flat — cell posting lists = durable
`DiskBTree`, centroids in a WAL-logged meta page; `CREATE INDEX ... USING
HNSW`/`IVF` builds it, `NEAR` reads it, **the async index worker is retired**
(`index_worker.rs` deleted, `IndexStatus` moved to `catalog.rs`), no format bump.
P3.d **large objects** (§ P3.d): out-of-line chunked + streamed `__lobs__` rows.
**`Engine::open` is now O(1) for every index type — zero rebuilding.** Crash
harness **14→19** (P13 B-Tree total-data-loss, P14 full-text, P15 edge, P16 large
object, P17 durable vector index recall-intact), no locked decision reversed. See
`docs/backlog/phase3_durable_storage.md` + `PROGRESS.md`'s P3.a–P3.d entries.
**Phase 4 COMPLETE (2026-07-09, SQL lane, branch `query-power`): query power.**
P4.a joins (hash + Grace spill / sort-merge / index-nested-loop over the durable
B-Tree) · P4.b aggregates + `GROUP BY`/`HAVING` + `ORDER BY` (external merge-sort
spill) + `DISTINCT` + `LIMIT`/`OFFSET` · P4.c scalar/`IN`/`EXISTS` subqueries
(correlated + uncorrelated) + `WITH` CTEs · P4.d `ANALYZE` (durable per-table
statistics, never recomputed on open) + cost-based optimizer (Selinger left-deep
DP join order + index-vs-scan) · P4.e `EXPLAIN [ANALYZE]`. Additive only: a
trivial single-table `SELECT` keeps its fast path; richer queries route through
a new `LogicalPlan::Query`/physical operator tree (`sql/{query,plan,query_exec,
join,aggregate,sort,optimizer,statistics,explain}.rs`). Correctness checked
differentially against SQLite. No `FORMAT_VERSION` bump, no new crash point
(no new storage mechanism — stats ride the existing catalog page), no locked
decision reversed. Known limits: window functions / recursive CTEs / `FULL
OUTER`+`USING`+`NATURAL` joins unsupported; the catalog (all TableDefs + stats)
is still a single ~8 KiB page blob, so a very wide analyzed schema can overflow
it (multi-page catalog is tracked tech debt). See `docs/backlog/
phase4_query_power.md` + `PROGRESS.md`'s Phase 4 entry.
**Phase 5 COMPLETE (2026-07-09, branch `p5e-concurrent-writers`): concurrency &
performance.** P5.a–P5.d (part 1, PR #14): thread-safe buffer pool (per-page
S/X latches), concurrent WAL append, `&self` transaction manager, and a real
lock manager (modes / blocking `Condvar` waits / wait-for-graph deadlock
detection). P5.e (part 2): `Heap`→`&self`, then **`Engine` is `Send + Sync`**
(all 6 mutated fields interior-mutable; `checkpoint::run` locks control only
off the fsync path), the server's single writer thread replaced by an
`Arc<Engine>` + `spawn_blocking` **worker pool**, heap read-modify-writes wired
to the per-page exclusive latch (no lost updates), and **group commit**
(`Wal::sync_up_to` leader runs `sync_all` with the append lock released) so
write throughput **scales with cores — 3.68× at 8 writers**
(`benches/concurrent_writers.rs`). A coarse `write_serial` lock serializes the
non-CRUD catalog/index write paths (documented limitation; only raw CRUD +
reads run concurrently). P5.f: per-query **timeouts / cancellation / `work_mem`**
(`query_limits.rs`, `Engine::execute_sql_with_limits`). Crash harness stays
**19/19**; sync invariant holds; no `FORMAT_VERSION` bump. See
`docs/backlog/phase5_concurrency.md` + `PROGRESS.md`'s Phase 5 entry.

**Phase 6 COMPLETE (2026-07-09, branch `phase6-ops-ha`): operations & HA** —
the roadmap's final phase, delivering a deployable single primary + read
replicas. **P6.a segmented WAL:** `db.wal/` is now a directory of fixed-size
16 MiB segment files (`seg-*.wal`, each with a base-LSN header); the active
segment seals + rotates on the size boundary, recovery scans all segments in
LSN order, and truncation deletes whole consumed segments instead of the old
rewrite-to-truncate — the enabler for concurrent WAL readers. Evolves **D6**
(data store stays single-file; WAL is now segmented — human sign-off recorded in
`PROGRESS.md`). **P6.b replication slots + WAL shipping:** a persisted
`SlotRegistry` (`slots.json`) pins a `restart_lsn`; the checkpoint truncation
floor becomes `min(checkpoint_lsn, min slot restart_lsn)`; `Wal::ship_from` /
`decode_stream` serialize the record stream a replica applies. **P6.c read
replicas + failover:** `replication::Replica` seeds from a **base snapshot** and
applies shipped WAL **incrementally on top** via the crash-recovery redo path
(no wipe — incremental redo on a populated store is normal crash recovery);
`promote()` opens it read-write for failover; `wait_for_sync_replicas` is the
optional synchronous-commit gate. (Documented limit: a page first allocated
*after* the base isn't full-page-image-covered, so roll-forward reconstructs
pages present in the base — re-base regularly.) **P6.d backups + PITR:**
`Engine::base_backup` (checkpoint + copy) + `archive_wal`, and
`backup::restore(base, archive, dest, target_lsn)` for point-in-time recovery
**by LSN** (time-based needs commit timestamps — a follow-up). **P6.e
users/roles/GRANT:** `authz::RoleStore` (`roles.json`) with transitive role
membership + per-table privileges; `Engine::execute_sql_as(user, ..)` enforces
them and intercepts the auth-DDL grammar (parsed in `authz`, not `sqlparser`);
the embedded API (`None`) is the implicit superuser, the server maps the JWT
`sub` claim to a user, and an empty role store is open/bootstrap mode
(backward compatible). **P6.f security:** native **TLS** (rustls via
`axum-server`, `server/tls.rs`) and an append-only **audit log** (`audit.log`);
**encryption-at-rest is DEFERRED** as a D9-sign-off-gated follow-up (it would
change the on-disk page format *and* fights the mmap page store). **P6.g
observability:** `Engine::stats()` (`pg_stat_*`-style: commits/aborts/
checkpoints/active-txns/WAL bytes/replication lag/data pages/recent slow
queries) + `GET /stats`, a slow-query log, and `docs/ops_runbook.md`. Crash
harness **19 → 21** (P18 segmented-WAL multi-segment recovery + truncation; P19
backup+PITR restore after primary loss). No `FORMAT_VERSION` bump; sync
invariant holds (no tokio/reqwest/axum/rustls in the default build). Benchmarks
(base backup 7 ms, restore 72 ms, PITR 43 ms, failover 26 ms at 5k rows) +
per-checkpoint detail in `PROGRESS.md`'s Phase 6 entry; ops in
`docs/backlog/phase6_ops_ha.md` and `docs/ops_runbook.md`.
**Commit-time WAL fsync (2026-07-09, branch `commit-time-fsync`): group-committed
force-log-at-commit is now the durability default on every path (§3.3).**
Statement mini-txns inside a user transaction defer their fsync; `Engine::commit`
issues the single coalesced commit fsync (fulfilling D1; D2/D5 unchanged, human
sign-off recorded in `PROGRESS.md`). Standalone ops self-sync (C1 audit);
eviction forces a WAL sync when no victim is durable, never `BufferPoolFull` (C2,
which also fixed two latent recovery pin-leak/durable-frontier bugs surfaced by a
small-pool memory-pressure test); WAL shipping is capped at the durable frontier
so replicas stay a prefix on failover (C3). Crash harness **21 → 25** (Pa–Pd) +
the valid-prefix property test now runs under both durability policies.
Acceptance: `benches/decompose.rs` shows the multi-model commit dropping ~33.1 ms
→ ~4.3 ms (~7.7×) at one fsync per commit, W0 at SQLite parity. No `FORMAT_VERSION`
bump; sync invariant holds. See `docs/backlog/commit_time_fsync.md`.
**Postgres baseline comparison (2026-07-09, branch `pg-baseline`)** and
**Autovacuum (2026-07-09, branch `autovacuum`, crash harness 25→26)** then
shipped (benches/ops only, and a background `std::thread` vacuum launcher). **Durable
on-disk FSM + catalog page-list (2026-07-10, branch `durable-fsm`): the SQL page
directory + free-space map move off the catalog blob into a per-table durable
`DiskBTree` keyed `page_id → free_bytes` (`TableDef.fsm_meta`), O(1) open, atomic
heap grow, vacuum-durable reclamation — closing the O(heap-pages) catalog-blob
`HeapFull` ceiling (§3.4/§8/§12; crash harness 26→28, P27+P28; no `FORMAT_VERSION`
bump). B-accept: marginal SQL-insert cost goes from rising-then-`HeapFull` (65→108→173
µs/row, dies at ~876 pages) to flat ~17–28 µs/row past 2,000 pages; concurrent-SQL
scaling unchanged (honest finding — `set_pages` was not the small-table bottleneck).
See `docs/backlog/durable_fsm_catalog_pagelist.md` + `PROGRESS.md`.**
**REST API enrichment (2026-07-11, item 12, branch
`claude/rest-api-enrichment-vly934`): multi-request transaction sessions over
HTTP (`X-Txn-Id`, per-session isolation, busy/principal/idle-reaper protection —
§8), one-shot `isolation` on `POST /sql`, RLS-over-REST via
`Engine::set_rls_policy_sql` (§4.2's RLS note), `POST /events/vacuum` +
`POST /admin/flush`, atomic base64 `POST /rows/batch`, and principal-bound
idle-expiring result cursors. Server-layer only — no format change, crash
harness stays 29; §8/§9 updated (including the stale writer-thread description,
corrected to the P5.e-3 `Arc<Engine>`/`spawn_blocking` shape).**
**MVCC abort-ordering fix (2026-07-12, item 16, branch `16-visibility-fix`):
`TransactionManager::abort` now keeps the aborting xid in the `active` set (and
its row locks held) for the whole physical undo, dropping it only afterward —
closing the window in which a concurrent snapshot classified an aborting txn's
not-yet-undone writes as committed (§4.1/§4.3 corrections). Root cause of the
item-16 MVCC visibility anomaly family (duplicate/missing rows, plus the
downstream D5-flush error and >120 s hang). No format change; crash harness
unchanged. See `docs/backlog/16_concurrent_sql_writes_visibility_anomaly.md`.**
**`UNIDB_CONCURRENT_SQL_WRITES` default-ON flip (2026-07-13, item 11, branch
`11-concurrent-writes-default-on`):** the concurrent SQL-write path soaked dark
behind the toggle; with the item-16 blocker fixed and the 28-cell concurrency
matrix green 28/28 at `CONC_REPEATS=10` (toggle on **and** off), the default is
now ON. `=0`/`false`/`off` (or `set_concurrent_sql_writes(false)`) forces the
serialized `cat_write` fallback. Table C re-measured on the flipped default:
indexed 8-writer 811 → 1016 commits/s. No format change; §5.2/§5.4 updated.
**Engine access & introspection contract (2026-07-13, Milestone 18, branch
`18-engine-access-contract-impl`):** the system catalog is now queryable as
synthesized virtual relations over the ordinary SQL surface —
`information_schema.{tables,columns,table_constraints,key_column_usage,
referential_constraints}` + `unidb_catalog.indexes` (`sql/information_schema.rs`;
resolved at plan time in `sql/plan.rs`, rows materialized in
`sql/query_exec.rs`). Read-only projection of existing `catalog.rs` metadata
(FK/PK/UNIQUE/CHECK already parse+persist since M11) — no storage, no crash
surface (harness stays 31), no `FORMAT_VERSION` bump. Reachable identically from
embed/attach/server. New reference doc `docs/engine_access_guide.md` (Application
Builder's Guide) stitches the access/query/type/error surface together; `GET
/tables` is superseded-but-kept. See
`docs/backlog/18_engine_access_contract.md` + `PROGRESS.md`.
Update alongside the next milestone's closeout.*
