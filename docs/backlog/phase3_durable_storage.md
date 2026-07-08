# Phase 3 — Multi-model durable storage (the moat)

## Status as of 2026-07-08: IN PROGRESS.
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
- P3.c (on-disk vector), P3.d (large objects): not started.

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

### P3.c — Durable on-disk vector index (the frontier item)
- HNSW (a RAM graph) doesn't page cleanly. Adopt an **on-disk ANN**: a
  DiskANN/Vamana-style on-disk graph with PQ-compressed vectors in RAM for
  routing, or an on-disk IVF. Bounded RAM, no full rebuild, crash-safe.
- **Start with a spike** to choose the approach and validate recall before
  committing — this is the hardest, possibly research-grade piece.
- Files: `vector.rs` rewrite / new `disk_vector` module.
- Tests: recall@k vs. the current HNSW baseline; RAM bounded under a large
  corpus; O(1) open; crash-safe.

### P3.d — Big-file / large-object storage (the "big file" differentiator)
- Values above a threshold stored **out-of-line** (TOAST-like): chunked into
  overflow pages / a large-object area, referenced by an out-of-line pointer in
  the tuple, and **streamed** — never load a whole multi-GB value into RAM.
- New streaming upload/download REST routes; the write is transactional (the
  blob + its row commit atomically); vacuum reclaims orphaned chunks.
- Files: new `large_object` module, `heap.rs` (out-of-line pointer form),
  `page.rs`, `catalog.rs` (large-object/BYTEA-toasted), `server/*` (streaming
  routes), `docs/REST_API.md`.
- Tests: store + stream a multi-GB blob without OOM; atomicity with the row;
  vacuum reclaims orphans; crash-recovery.

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
