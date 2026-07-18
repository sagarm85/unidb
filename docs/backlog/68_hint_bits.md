**Type:** Performance
**Status:** ⏳ NOT STARTED

# Item 68 — Hint bits

## Problem

Every visibility check on a live tuple reads `xmin`/`xmax` from the tuple header and
then walks the in-memory transaction table to determine committed/aborted status. For
committed tuples (the common case in a read-heavy workload) this is a redundant table
lookup on every scan pass.

Postgres solves this with *hint bits*: two status bits in the tuple header
(`HEAP_XMIN_COMMITTED`, `HEAP_XMAX_COMMITTED`, etc.) that are set lazily the first
time a tuple is confirmed visible/invisible. After that, the committed-status check
short-circuits without any table lookup.

## Expected gain

- Reduces per-row scan cost on read-heavy workloads (SELECT, range scans, aggregate).
- Eliminates redundant `txn_state(xmin)` / `txn_state(xmax)` lookups for already-
  committed tuples — the common case in steady-state tables.
- Estimated SELECT gain: ~5–10% at large row counts where scan dominates.

## Design notes

- The hint bits are a **dirty write**: setting them does NOT require a WAL record.
  The page will be re-set correctly from the transaction table on any crash + recovery.
  This is standard practice (Postgres, MySQL InnoDB) and is safe.
- Existing `TupleHeader` has reserved bytes in MVCC fields — check whether the current
  24B layout can accommodate 2 additional status bits without a `FORMAT_VERSION` bump.
  If the reserved padding already exists, this may be zero-format-cost.
- Implementation: in `get_visible` / `get_visible_with_rid`, after resolving committed
  status from the txn table, write back the hint bit to the page frame (no WAL).
- Gate the hint-bit write behind a `cfg(debug_assertions)` correctness check that
  the page CRC is not re-validated after a hint-bit set (hint bits bypass CRC logic).

## Acceptance criteria

- SELECT throughput increases by ≥5% at 100k-row scale (Docker bench comparison).
- No crash-recovery correctness regression (existing 50 crash tests + new hint-bit
  specific test showing hint bits are correctly re-derived after crash).
- No `FORMAT_VERSION` bump required (use existing reserved bytes if available).

## Dependencies

- Supersedes none; complements items 54, 59 (decode pushdown already shipped).
- Can be developed independently of items 69, 70.
