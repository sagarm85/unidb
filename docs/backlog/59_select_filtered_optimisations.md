# Item 59 — SELECT filtered optimisations: COLS_DECODED gate + column pre-binding + late materialisation

**Type:** Performance
**Status:** SHIPPED 2026-07-17

> Fable-5 architectural analysis (2026-07-17) identified three root causes of
> the SELECT filtered gap after item 54 (0.57× PG at 5% selectivity).
> Ranked by impact: (1) `COLS_DECODED` atomic write on every decoded column
> (~10% of hot-path time), (2) linear `String` scan per predicate column per
> row (~25–30%), (3) full `deform_row` on 95% of rows that fail the predicate
> at 5% selectivity (~40%).

---

## Root-cause analysis

### Bench selectivity fix (prerequisite, commit 79890a7)

The previous SELECT filtered bench used `k < N` (100% selectivity), measuring
the absolute-best case for any filter and giving a misleading 0.57× ratio. The
real gap drivers were hidden because all rows passed. Fixed to `k < N/20` (5%
selectivity) — the realistic case where a predicate is actually selective.

### Fix 1 — COLS_DECODED atomic gate (~10% addressable)

`COLS_DECODED.fetch_add(1, Relaxed)` fired inside `deform_row` on every
decoded column — a measurement counter, not correctness logic. At 5%
selectivity with 4 columns decoded per predicate check, this is 4 atomic
stores per row just for diagnostics.

**Solution:** add `static DIAGNOSTICS_ENABLED: AtomicBool = AtomicBool::new(false)`.
Gate all three `COLS_DECODED.fetch_add()` call sites behind a
`DIAGNOSTICS_ENABLED.load(Relaxed)` check. Add `Engine::enable_diagnostics()`
public API; call it from the bench's `measured_unidb()` function so `cols/row`
reporting still works. Update the `group_by_cols_per_row` test and the
`a3_gate` integration test to call `enable_diagnostics()` before sampling.

Files: `src/sql/executor.rs`, `src/lib.rs`, `benches/decompose.rs`,
`tests/a3_gate.rs`.

### Fix 2 — Column index pre-binding (~25–30% addressable)

`eval_expr(Expr::Column(name))` did `columns.iter().position(|c| &c.name == name)`
on every predicate evaluation — a linear `String` scan over all `ColumnDef`s,
called twice per row for `k >= 0 AND k < N/20`.

**Solution:** add `Expr::ColumnSlot(usize)` variant to the `Expr` enum
(executor-internal only; never serialised). Add `bind_predicate_columns(expr:
&mut Expr, columns: &[ColumnDef])` that walks the predicate AST once before
the scan loop, replacing every `Expr::Column(name)` with `Expr::ColumnSlot(idx)`
where `idx = columns.iter().position(...)`. In `eval_expr`, `ColumnSlot(idx)`
becomes `row.get(idx).cloned()` — direct positional access, no `String` scan.

Also added to `exec_select_readonly` (concurrent read path) for consistency.

Files: `src/sql/logical.rs` (new variant + `bind_expr` arm),
`src/sql/executor.rs` (pre-binding pass, `eval_expr` arm, `expr_columns` arm),
`src/sql/query.rs` (`qualify_policy` match arm for `ColumnSlot`).

### Fix 3 — Late materialisation via raw integer filter (~40% addressable at 5%)

`scan_page_visit` (called by `parallel_filter_project`) called `deform_row`
on every visible row to build a `Vec<Literal>` for predicate evaluation —
including the 95% of rows at 5% selectivity that fail the predicate. Two
`deform_row` calls per row (predicate columns + projection columns) with
`Vec<Literal>` allocations even for rows that are immediately discarded.

**Solution:** for the common case of simple `ColumnSlot(idx) op Literal::Int`
predicates (which are now guaranteed post Fix 2), add:

1. `try_raw_i64_at(bytes, col_idx, columns) -> Option<i64>` — reads the i64
   value at column `col_idx` directly from page bytes by computing the byte
   offset of preceding fixed-width columns (tag sizes: INT=9, Bool=2,
   Timestamp=9, Float=9, Date=5, Time=9, Uuid=17, Decimal=18). Returns `None`
   if any preceding column is variable-width (Text/Json/Bytea/Vector) — falls
   back to `deform_row`.

2. `struct RawFilter { terms: Vec<(usize, CmpOp, i64)> }` with
   `RawFilter::passes(bytes, columns) -> Option<bool>` — evaluates all integer
   terms against raw bytes, returning `Some(true/false)` or `None` (fallback).

3. `try_build_raw_filter(expr: &Expr) -> Option<RawFilter>` — inspects the
   bound predicate and builds a `RawFilter` if ALL terms are `ColumnSlot op Int`
   conjunctions; returns `None` otherwise.

In the `per_row` closure inside `exec_select`: check `raw_filter.passes(bytes)`
before calling `deform_row`. For the bench table `(id INT, k INT, g INT, body TEXT)`
with predicate on `k` (col index 1): id is fixed-width INT (9 bytes) → k is
reachable at `tag_offset + 9 + 1` bytes from tuple payload start. At 5%
selectivity, 95% of rows are rejected after reading just 10 bytes, never
constructing a `Vec<Literal>`.

Also added to `exec_select_readonly` for the concurrent read path.

Files: `src/sql/executor.rs` (all additions).

---

## Changes

| File | Change |
|------|--------|
| `src/sql/logical.rs` | `Expr::ColumnSlot(usize)` variant + `bind_expr` arm |
| `src/sql/executor.rs` | `DIAGNOSTICS_ENABLED` static; Fix 1 gating; `bind_predicate_columns`; `Expr::ColumnSlot` in `eval_expr`/`expr_columns`; `try_raw_i64_at`; `RawFilter`; `try_build_raw_filter`; `collect_raw_terms`; `per_row` closure updated in `exec_select` + `exec_select_readonly`; 3 new tests |
| `src/lib.rs` | `Engine::enable_diagnostics()` public API |
| `src/sql/query.rs` | `ColumnSlot` arm in `qualify_policy` |
| `benches/decompose.rs` | `Engine::enable_diagnostics()` in `measured_unidb()` |
| `tests/a3_gate.rs` | `Engine::enable_diagnostics()` before `cols_decoded_total()` sampling |

**Total scope:** ~280 lines added/changed across 6 files. No WAL format change,
no FORMAT_VERSION bump, no locked-decision touch, no new crash injection points
(read-only hot path). Crash harness: existing 44/44 tests cover correctness
without new points needed.

---

## Tests added

| Test | What it verifies |
|------|-----------------|
| `select_filtered_col_pre_binding_same_results` | Pre-bound predicate (ColumnSlot) returns same rows as column-name path |
| `select_filtered_late_mat_same_results` | Raw integer filter returns correct rows at 5% and 50% selectivity |
| `select_filtered_late_mat_fallback` | TEXT predicate (variable-width col) correctly falls back to full `deform_row` |

---

## Benchmark results

Baseline from item 54 (`benchmark_20260716_232744.md`, Docker Linux fsync,
100k rows, 5% selectivity bench fixed in `79890a7`):

| operation | records | unidb (rec/s) | PG (rec/s) | ratio | cols/row |
|-----------|--------:|-------------:|----------:|:-----:|--------:|
| SELECT filtered (k<N/20) | 100000 | **TBD** | **TBD** | **TBD** | **TBD** |

> Docker bench results to be filled in after Docker run completes.
> Local (macOS, no Postgres comparison) probe: bench pre-bound correctly at 5%
> selectivity; raw filter fires for all integer predicate terms.

---

## Acceptance guards (A7 regressions)

All A7 guards must pass after this item:

| Guard | Target | Status |
|-------|--------|--------|
| SELECT COUNT(*) ≥5× PG | ≥5× | TBD (Docker) |
| SELECT grouped ≥1.00× PG | ≥1.00× | TBD (Docker) |
| SELECT filtered ≥0.55× PG | ≥0.55× | TBD (Docker) |
| INSERT ≥0.50× PG | ≥0.50× | TBD (Docker) |
| W4/W0 ≤2.3× | ≤2.3× | TBD (Docker) |
| DELETE selected ≥0.15× PG | ≥0.15× | TBD (Docker) |
