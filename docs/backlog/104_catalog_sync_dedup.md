# Item 104 — Catalog sync dedup: remove double-fsync per INSERT

**Type:** Performance
**Status:** SHIPPED (→ PROGRESS.md "Item 104 — Catalog sync dedup")

## Problem

Every INSERT triggered two WAL fsyncs under group-commit (server) mode:

1. The row commit fsync — correct, required for D5 durability.
2. `wal.sync_up_to(catalog_lsn)` immediately after `catalog.persist_only()` in
   `Engine::commit` — added by item 97 to advance `durable_lsn` so the
   catalog mini-txn appeared in the WAL replication stream.

The second fsync was called **outside the group-commit window**. Unlike the
commit fsync (which coalesces N concurrent committers behind one `sync_all`),
the catalog fsync ran synchronously per-commit. Under 32 concurrent writers this
was effectively a serialization point that halved INSERT throughput.

The `persist_only` call itself was also still writing a WAL mini-txn for the
catalog update on every commit, adding WAL bandwidth even when no replication
slot was active.

## Solution

Remove both `wal.sync_up_to(catalog_lsn)` AND `catalog.persist_only()` from
`Engine::commit`. Removing only the fsync while retaining `persist_only()` caused
a replication regression: `persist_only()` atomically flips `catalog_root` in the
control file per commit; without the matching `sync_up_to`, the catalog WAL records
weren't included in the shipped WAL stream. The replica adopted the new
`catalog_root` (via `adopt_control`) but had never received the catalog page it
pointed at → `SlotOutOfRange`. The correct fix is to update `row_count` in-memory
only in the commit path and persist the full catalog (WAL mini-txn + `catalog_root`
flip) only at checkpoint. The catalog `row_count` is now durable only at checkpoint
time — matching how Postgres handles `pg_class.reltuples`.

**Changed contract:** `row_count` is approximate on disk (checkpoint-granularity)
but exact in-memory for the lifetime of the process. After a crash (no
checkpoint), the recovered catalog's `row_count` may be stale.

### Crash recovery handling (ROW_COUNT_UNKNOWN)

`Catalog::load` now resets every table's `row_count` to `ROW_COUNT_UNKNOWN =
i64::MIN` on load. The item 97 O(1) `COUNT(*)` fast path in `query_exec.rs`
detects this sentinel and falls through to `Heap::count_visible` (an exact heap
scan). The result is cached back into the in-memory catalog (when the catalog
handle is Exclusive — i.e., non-concurrent mode) so subsequent COUNTs are O(1)
again. In concurrent mode (`CatalogHandle::Shared`), the cache write is skipped
and every COUNT falls back to heap scan until the next checkpoint persists the
true count.

The commit path (`Engine::commit`) guards against applying deltas to an UNKNOWN
base: when `row_count == ROW_COUNT_UNKNOWN`, the delta is skipped. This prevents
`i64::MIN + delta = wrong_value` from corrupting the in-memory count. After the
first COUNT calibration (which caches the exact count in non-concurrent mode),
subsequent DML deltas are applied correctly.

## Key invariant

`COUNT(*) FROM t` returns the exact count of committed visible rows at all times.
The optimization is only about when the count is flushed to disk.

## Files changed

- `src/catalog.rs`: added `ROW_COUNT_UNKNOWN` constant; `Catalog::load` calls
  `reset_row_counts_unknown()` after parsing; new `reset_row_counts_unknown()`
  helper.
- `src/lib.rs`: import `ROW_COUNT_UNKNOWN`; removed `wal.sync_up_to(catalog_lsn)`
  AND `catalog.persist_only()` from `Engine::commit`; row_count delta applied
  in-memory only; guarded `saturating_add` with `!= ROW_COUNT_UNKNOWN`.
- `src/sql/query_exec.rs`: import `ROW_COUNT_UNKNOWN`; extended COUNT(*) fast
  path to skip when UNKNOWN and run calibration heap scan instead.
- `tests/crash/main.rs`: added `p104_catalog_sync_dedup_crash_recovery_count_exact`.

## Acceptance criteria

1. ≥ 1.3× INSERT throughput gain at 32 concurrent writers (Docker bench pending).
2. `COUNT(*) FROM t` always returns exact committed count (verified by P104).
3. After crash recovery, first `COUNT(*)` triggers heap scan (tracing log visible).
4. All crash harness tests pass (P104 + existing tests).
5. All unit tests pass; `cargo clippy -- -D warnings` clean.
