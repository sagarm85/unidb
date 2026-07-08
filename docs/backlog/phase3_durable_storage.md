# Phase 3 — Multi-model durable storage (the moat)

## Status as of 2026-07-08: NOT STARTED.

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

### P3.a — Durable B-Tree index
- Replace the in-memory `BTreeIndex` with an on-disk B-tree: nodes are pages in
  the page store, buffer-pool-managed, mutations **WAL-logged** and
  crash-recovered — **no rebuild on open**.
- Files: `btree_index.rs` (disk impl), `page.rs`/`bufferpool.rs`/`wal.rs`/
  `recovery.rs`, `index_worker.rs` (stop rebuilding B-Tree), `lib.rs` (drop
  B-Tree from `rebuild_secondary_indexes`).
- Tests: open-time independent of table size; crash mid-node-split recovers;
  index-assisted `SELECT` unchanged in behavior.

### P3.b — Durable inverted + CSR + EdgeIndex
- Same treatment for `fulltext.rs` (inverted), `csr_index.rs` / `graph/index.rs`
  (CSR/EdgeIndex): persist as pages, WAL-logged, no rebuild. Removes
  `rebuild_edge_index` / `rebuild_csr_index` and the full-text rebuild.
- Tests: reopen with no rebuild; crash-recovery of each; graph traversal +
  full-text query unchanged.

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
