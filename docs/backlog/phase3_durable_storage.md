# Phase 3 — Multi-model durable storage (the moat)

## Status as of 2026-07-09: COMPLETE.
**Every secondary index is durable and crash-recovered, and `Engine::open` does
ZERO index rebuilding — the O(1)-open moat is real.** P3.a (B-Tree), P3.b
(full-text + edge; CSR retired), P3.c (on-disk vector — spike **and** production
wiring), and P3.d (large objects) are all shipped. The async index worker is
retired (its last user, the in-RAM HNSW vector index, is gone). Crash harness
grew 14 → **19** (P13–P17). See the per-checkpoint sections below and
`PROGRESS.md`.

### Historical status as of 2026-07-08: IN PROGRESS.
- **P3.a — Durable paged WAL-logged B-Tree: SHIPPED** (branch `durable-storage`).
  See `PROGRESS.md` → "P3.a" and `MEMORY.md`. The B-Tree is now on-disk,
  buffer-pool-managed, WAL-logged (`WAL_INDEX`), crash-recovered, and **no
  longer rebuilt on open** — removed from `rebuild_secondary_indexes`. Crash
  harness grew 14 → 15 (new point P13). `FORMAT_VERSION` 4 → 5.
- **P3.b — Durable inverted (full-text) + edge index; CSR retired: SHIPPED**.
  Both full-text and the edge-adjacency index are now durable `DiskBTree`s
  (reusing all of P3.a's `WAL_INDEX` machinery — no new format version): the
  full-text index keys on tokens, the edge index keys on `__edges__.from_id`.
  Removed `rebuild_edge_index` + full-text rebuild from `rebuild_secondary_
  indexes`. **CSR was retired** (removed `rebuild_csr_index` + its warm-keeping
  writes) — it was consulted by no read path since the M7 traversal-uses-CSR
  revert, and adjacency is now served durably by the edge index. The async
  worker now serves only the vector (Hnsw) index. New Rust-API read path
  `Engine::search_fulltext`. Crash harness 15 → 17 (P14 full-text, P15 edge).
- **P3.c — On-disk vector index: SHIPPED** (spike 2026-07-08, production
  2026-07-09; `docs/design/p3c_vector_spike.md`). **On-disk IVF-Flat**
  (`src/disk_vector.rs`) — cell posting lists are a durable `DiskBTree` (reuses
  P3.a), centroids + config in a WAL-logged meta page (crash-recovered, bounded
  RAM). `CREATE INDEX ... USING HNSW`/`IVF` builds it, `NEAR` reads it, the async
  worker is retired, crash point **P17** added. Recall@10 = 1.000 at nprobe≥4
  matching HNSW. **The spike also found + fixed a real `DiskBTree` duplicate-key
  bug** (a run straddling a leaf boundary under-returned) affecting P3.a/P3.b. See
  the per-checkpoint P3.c section below for the production details.
- **P3.d — Large-object storage: SHIPPED** (embedded API). Values are stored
  **out-of-line, chunked (~7 KiB), and streamed** — a large object is a sequence
  of chunk rows in a `__lobs__` system table indexed by a durable `DiskBTree` on
  `lob_id` (reuses P3.a). `Engine::put_large_object`/`read_large_object`/
  `delete_large_object` stream one chunk at a time (multi-GB without OOM), are
  **atomic with the caller's transaction** (chunks are ordinary MVCC/WAL rows),
  crash-recovered (crash point **P16**), and vacuum reclaims deleted/orphaned
  chunks. Follow-up (documented): transparent BYTEA-toast + streaming REST routes.

Kills rebuild-on-open + the RAM ceiling, and owns the AI/big-file story
Postgres doesn't have. Companion to [`roadmap.md`](roadmap.md) §4. **Depends on
Phase 1** (buffer pool + FSM + WAL full-page-writes) being solid — durable
indexes are new WAL-logged paged structures. Mostly Core lane + new modules.

## Context

Today *every* secondary index (B-Tree, inverted, CSR, HNSW, EdgeIndex) is
in-memory and rebuilt on open — O(all data) startup, and everything must fit in
RAM. At 100s of GB that means minutes-to-hours startup and OOM. Real databases
keep indexes durable on disk, buffer-pool-managed, WAL-recovered. This phase
makes that true — and makes the **vector** and **big-file** paths first-class,
which is the differentiator.

## Scope

- **IN:** durable paged B-Tree / inverted / CSR / EdgeIndex; durable on-disk
  vector index; big-file / large-object storage.
- **OUT:** cost-based use of these (Phase 4); concurrent access to them
  (Phase 5); S3 tiering (parked).

## Checkpoints

### P3.a — Durable B-Tree index — SHIPPED (2026-07-08)
- Replaced the in-memory `BTreeIndex` with an on-disk B+tree (`DiskBTree`):
  nodes are pages in the page store, buffer-pool-managed, mutations **WAL-logged**
  as full node-page images (new redo-only `WAL_INDEX`) and crash-recovered —
  **no rebuild on open**. A stable per-index meta page (id stored in the catalog
  as `ColumnDef.index_root`) points at the current root, so a root split never
  rewrites the catalog. Moved off the async worker onto the synchronous
  writer/read path (like `EdgeIndex`).
- Files touched: `btree_index.rs` (disk impl), `format.rs` (`WAL_INDEX`,
  `PAGE_TYPE_BTREE`, `FORMAT_VERSION` 4→5), `wal.rs` (`log_index`),
  `bufferpool.rs` (`page_size()` accessor), `recovery.rs` (`WAL_INDEX` redo),
  `catalog.rs` (`index_root` + `set_column_index_root`), `heap.rs` (`get_raw`
  for vacuum), `index_worker.rs` (BTree removed), `sql/executor.rs` (durable
  write + read path), `lib.rs` (dropped from `rebuild_secondary_indexes`;
  vacuum scrubs the durable tree), `tests/crash` (P13), `benches/durable_index.rs`.
- Tests: open independent of table size (`benches/durable_index.rs`); crash mid
  node-split / total data-file loss recovers from the WAL (crash P13); aborted
  insert never surfaces via the index (MVCC re-check); durable reopen without
  rebuild; module-level split/range/reopen tests.

### P3.b — Durable inverted + edge index; CSR retired — SHIPPED (2026-07-08)
- **Full-text (inverted)** and the **edge-adjacency index** are now durable
  `DiskBTree`s — reusing P3.a's `WAL_INDEX` machinery, no new format version.
  Full-text keys on tokens (`fulltext::tokenize`), one `(token, RowId)` entry
  per token; the edge index keys on `__edges__.from_id` (`OrderedValue::Int`).
  Both are read from disk on open — no rebuild. Removed `rebuild_edge_index` and
  the full-text branch of `rebuild_secondary_indexes`.
- **CSR retired.** `csr_index.rs` was consulted by no read path after M7's
  traversal-uses-CSR revert, and adjacency is now served durably by the edge
  index, so its rebuild-on-open + warm-keeping were removed (`rebuild_csr_index`
  deleted; no `IndexedColumn::Edge` sent). The module + its benchmark remain but
  are no longer wired into the runtime. The async worker now serves only Hnsw.
- New read path `Engine::search_fulltext` (Rust API) — the durable full-text
  index previously had no query surface at all.
- Tests: durable reopen without rebuild (`tests/btree_mvcc.rs`,
  `tests/index_rebuild.rs`, `tests/graph_rebuild.rs`); crash-recovery of each
  (crash points P14 full-text, P15 edge); graph traversal + full-text query
  unchanged. `search_fulltext` unit tests. `benches/durable_index.rs` gains an
  edge-index reopen-cost table.

### P3.c — Durable on-disk vector index — SHIPPED (spike 2026-07-08, production 2026-07-09)
- **Spike done (2026-07-08).** Chose **on-disk IVF-Flat** over DiskANN/Vamana for
  v1: its only on-disk state is a cell posting list (`cell_id → [RowId]`), which
  is exactly a `DiskBTree` (P3.a) — so it is already durable, WAL-logged,
  crash-recovered, and bounded-RAM (just the centroid table), with no new storage
  format. DiskANN is parked as a higher-recall option behind the same interface.
  Full rationale + numbers: `docs/design/p3c_vector_spike.md`. The spike also
  surfaced + fixed a real `DiskBTree` duplicate-key-spanning-leaves bug
  (regression-tested).
- **Production wiring shipped (2026-07-09).** `DiskIvfIndex` is now a durable,
  stateless handle over a **stable meta page** (id stored in
  `ColumnDef.index_root`, like `DiskBTree`): the meta page records
  metric/dim/nlist/nprobe + the postings tree's meta page + a WAL-logged centroid
  page chain, so the centroid table is **crash-recovered, never recomputed** and
  the index is read straight from disk on open (bounded RAM, O(nlist·dim)).
  - `CREATE INDEX ... USING HNSW` (and the new `USING IVF` alias) builds it:
    train centroids from the committed rows, persist meta+centroids, insert each
    row into its cell. An empty-table `CREATE INDEX` trains one origin cell
    (correct-but-flat until re-created — documented).
  - `NEAR` routes through it: probe the `nprobe` nearest cells' posting lists →
    exact re-rank from the heap's stored vectors → MVCC/RLS/predicate re-check
    (identical over-fetch-then-filter contract as before).
  - `apply_durable_index_writes` maintains it on every INSERT/UPDATE; vacuum's
    aliasing gate scrubs it (`DiskIvfIndex::remove`).
  - **The async index worker is retired** — its last user was the in-RAM HNSW.
    `rebuild_secondary_indexes` is deleted; `Engine::open` does ZERO index
    rebuilding. `IndexStatus` moved to `catalog.rs`; a durable index is always
    `Ready`.
- **Recall validated** (`benches/vector_recall.rs`): recall@10 = **1.000 at
  nprobe≥4** vs. brute-force ground truth, matching the HNSW baseline's 1.000, at
  4 KB RAM / ~34 ms build vs. HNSW's 30 s build for 1,200 vectors. A larger
  20,000-vector × 64-dim sweep holds recall@10 = 1.000 at bounded ~36 KB RAM, and
  a reopen-by-meta-page check confirms identical recall with no rebuild.
- New crash point **P17** (durable vector index survives a crash, recall intact —
  harness 18 → **19**). No `FORMAT_VERSION` bump (reuses `WAL_INDEX` +
  `PAGE_TYPE_BTREE`).
- Files: `src/disk_vector.rs` (production `DiskIvfIndex`), `src/sql/executor.rs`
  (CREATE INDEX + NEAR + maintenance), `src/lib.rs` (open/vacuum/index_status;
  worker removed), `src/catalog.rs` (`IndexStatus`), `src/sql/parser.rs` (`USING
  IVF`), `tests/crash/main.rs` (P17), `benches/vector_recall.rs`,
  `docs/design/p3c_vector_spike.md`.

### P3.d — Big-file / large-object storage — SHIPPED (2026-07-08, embedded API)
- Values are stored **out-of-line, chunked (~7 KiB/chunk), and streamed** — a
  large object is a sequence of chunk rows in a `__lobs__` system heap table,
  indexed by a durable `DiskBTree` on `lob_id` (reuses P3.a). Because chunks are
  ordinary MVCC/WAL rows written under the caller's `xid`, the blob is **atomic
  with the transaction** and crash-recovered with **zero new storage format** —
  the deliberate design choice over a bespoke overflow-page format.
- API (`src/large_object.rs` + `Engine`): `put_large_object(xid, impl Read) ->
  lob_id`, `read_large_object(xid, lob_id, impl Write)`, `delete_large_object`.
  Both paths hold **one ~7 KiB chunk at a time** — a multi-GB value never loads
  whole (the "without OOM" gate). Deleted/orphaned chunks are reclaimed by the
  ordinary heap vacuum (M10).
- Files: new `src/large_object.rs`, `lib.rs` (Engine API + open wiring +
  `derive_next_lob_id`), `tests/large_object.rs`, `tests/crash` (P16).
- Tests: 5 MiB store→stream round-trip (checksum, O(1) memory); atomicity
  (aborted blob invisible); vacuum reclaims deleted chunks; crash-recovery (P16).
- **Deferred follow-ups (documented, not silent):** transparently toasting a
  large inline `BYTEA` column value to this store; streaming REST upload/download
  routes (server-side streaming through the single writer thread needs a chunked
  command path — a real design piece, not buffering a whole blob in the writer).

## Locked decisions touched

| Decision | Effect |
|---|---|
| D1 / D5 / D9 | Indexes + large objects become WAL-logged + crash-recovered — strengthens durability; `FORMAT_VERSION` bump |
| D4 (forward-compatible tuple) | Tuple gains an out-of-line-pointer form; old rows still decode |
| D6 / D8 | Unchanged (still single file, 8 KiB pages) |

## Verification gates (Phase 3 done =)

- `Engine::open` is O(1) regardless of data size — **no index rebuild**.
- Index memory is bounded (buffer-pool-managed), not O(data).
- Vector recall benchmark vs. the in-RAM baseline; big-file streaming without
  OOM; new crash-injection points for every durable index + large objects.
- No point-lookup perf regression; `clippy -D warnings` + `fmt` clean; PR per
  checkpoint; `PROGRESS.md`/`MEMORY.md` updated.

## Known limitations / deferred

- The disk vector index may ship as a single v1 approach, tuned later.
- Cross-index transactional consistency rides on the WAL (same as the heap).
- S3 / tiered cold storage is Phase 6 / parked, not here.
