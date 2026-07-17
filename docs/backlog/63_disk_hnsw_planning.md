**Type:** Performance
**Status:** SHIPPED — 2026-07-17, branch `63-disk-hnsw` (replaces IVF-Flat for IndexKind::Hnsw)

## Summary

Design and implement an on-disk HNSW index to replace the current IVF-Flat
index for high-recall approximate nearest-neighbor search at corpus sizes ≥ 100k
rows, where the IVF-Flat approach (`nlist` capped at 256, `nprobe` = 32 →
3.2% scan at 1M rows) degrades to recall@10 < 0.80.

## Item 62 gate unlock (2026-07-17)

IVF-Flat recall@10 at 100k rows = 0.421 (target ≥ 0.90). Gate unlocked. Disk
HNSW required.

## What shipped

- `src/hnsw_index.rs` — `DiskHnswIndex` struct with:
  - O(log N) beam search (ef_construction=200, ef_search=50)
  - Nodes stored in fixed-size slots (712 bytes for dim=128, M_max0=32) in
    WAL-logged base pages (reuses `WAL_INDEX` = no FORMAT_VERSION bump)
  - Node lookup via `node_index` DiskBTree (heap RowId → node page/slot)
  - Upper-layer connections (layers > 0) via `upper_layer` DiskBTree
  - Layer-0 connections inline in node page slots (M_max0=32, 192 bytes)
  - PRNG-based level assignment (geometric distribution, M_L = 1/ln(16))
  - Reciprocal connections with heuristic shrink at M_max0/M limits
  - Entry-point stored directly in meta (node_page + node_slot) for
    crash-safe recovery without node_index lookup
  - `remove()` intentionally no-op (MVCC visibility filters dead rows)
- `src/sql/executor.rs` — wired into exec_create_index, apply_durable_index_writes,
  exec_select_near, stage_row_index_writes_update
- `src/lib.rs` — vacuum paths use DiskHnswIndex.remove()
- `tests/crash/main.rs` — P60a (node + meta survive crash) and P60b
  (post-checkpoint inserts survive crash) added; P17 exercises full HNSW path

## Acceptance criteria (met/not yet measured)

- recall@10 ≥ 0.95 at 1k rows: unit test validates ≥ 0.80 at 200×dim32 (debug);
  release bench needed for 1k/10k/100k×dim128 (pending `cargo bench --bench decompose`)
- Crash harness: P17 + P60a + P60b: 48/48 PASS
- clippy -D warnings: CLEAN
- cargo fmt --all: CLEAN
- 431 lib unit tests + 48 crash tests: all PASS

## Notes

- No FORMAT_VERSION bump: reuses WAL_INDEX (full-page images) — existing recovery
  handles HNSW pages identically to BTree pages.
- `DiskIvfIndex` is retained in `disk_vector.rs` for any on-disk databases
  created with the old IVF-Flat format; they can be re-indexed with
  `CREATE INDEX ... USING HNSW` after upgrade. New creates use HNSW.
- Pending: release-mode recall bench at 1k/10k/100k rows (scripts/report.sh or
  `cargo bench --bench decompose UNIDB_BENCH=ivf_validate`).
