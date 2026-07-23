# UPDATE/DELETE: predicate-column decode pushdown on matching_rows (item 47 Phase B)

**Type:** Performance
**Status:** ✅ SHIPPED 2026-07-16 (PR #131) — Step 1 (DELETE decode pushdown) shipped; Step 2 found NO-OP for UPDATE (structural, see correction note below). See `backlog_index.md` row 52 / PROGRESS.md. _(Header normalized 2026-07-22.)_

_This is the official tracking item for item 47 Phase B, which was specified as an open follow-on in `47_update_delete_write_throughput.md` after Phase A shipped (PR #119, 2026-07-16)._

## Measured gap (`030325`, Docker Linux fsync, 2026-07-16)

| operation | records | unidb (rec/s) | PG (rec/s) | ratio | dec/row | cols/row |
|-----------|--------:|-------------:|----------:|:-----:|--------:|---------:|
| UPDATE bulk (k<N/2) | 5000 | 115,549 | 832,680 | **0.14×** | 1.00 | **8.00** |
| DELETE selected (k>=N) | 10000 | 614,222 | 3,792,487 | **0.16×** | 1.00 | **6.00** |

Platform: aarch64 · 18 cores · Linux 6.12.76-linuxkit. Both engines `fsync` (matched durability).

`cols/row=8.00` on UPDATE and `cols/row=6.00` on DELETE are the diagnostic signal: the `matching_rows` scan path decodes every column in every candidate row, even though the predicate evaluates only one column (`k`). The SELECT path already has B2 decode-pushdown (`deform_row(pred_upto, pred_needed)` in `matching_rows` / `executor.rs`). This item extends that same mechanism to the write path.

## Estimated gains after this item

- **DELETE selected: 0.16× → ~0.30–0.40× PG.** DELETE does not produce a new row version — it only writes an xmax stamp. After collecting matching RowIds, zero column values are needed. Reducing `cols/row` from 6 → 0 on the scan phase eliminates a large fraction of DELETE's scan cost.
- **UPDATE bulk: 0.14× → ~0.18–0.22× PG.** UPDATE still needs all columns to produce the new heap version (insert-new-version MVCC). Phase B reduces the predicate-scan decode (cols/row: 8 → ~2 on non-matched rows) but the write step's full-row decode is unavoidable without Phase C (HOT chain, locked decision). Improvement is real but bounded.

## Root cause

`matching_rows` in `src/sql/executor.rs` calls `decode_row(page, slot)` (full decode, all columns) for every candidate row before applying the predicate filter. The SELECT path calls `deform_row(page, slot, pred_upto, pred_needed)` instead — it stops deforming at the last predicate column and only materializes the columns actually referenced by the predicate. This column-mask optimisation (B2) was applied to SELECT in item 47's predecessor work but was not extended to the write path.

For DELETE: a predicate-only decode on the scan phase, then zero decode on the write phase (only `slot.xmax` is stamped). The full 6-column decode is entirely wasted.

For UPDATE: a predicate-only decode on the scan phase (1–2 columns for `WHERE k<N/2`), then full 8-column decode on matched rows only (needed to produce the new version). Net change: non-matched rows go from 8-col decode to 1-col decode; matched rows still 8-col.

## Plan

All changes are in `src/sql/executor.rs` (and the helper it calls: `deform_row` in `src/heap.rs`, already available for SELECT's B2 path).

### Step 1 — DELETE: predicate-only decode on matching_rows
In `exec_delete` / the `matching_rows` call site for DELETE:
- Pass `pred_upto` / `pred_needed` mask (derived from the DELETE WHERE clause) to `matching_rows`.
- `matching_rows` calls `deform_row(page, slot, pred_upto, pred_needed)` instead of `decode_row`.
- After collecting the matched RowId list, no further decode is needed — xmax stamp operates on the raw slot bytes.
- Expected: `cols/row` for DELETE drops from 6 → ~1.

### Step 2 — UPDATE: predicate-only decode on the scan phase
In the `matching_rows` call site for `exec_update`:
- Same mask derivation as Step 1 for the scan phase.
- Matched rows still call `decode_row` (full decode) to read current values for producing the new version.
- Expected: `cols/row` for UPDATE drops from 8 → ~(1 + matched_fraction × 8). At 50% selectivity, ~4.5 weighted cols/row.

### Step 3 — Measure and record
Run `scripts/report.sh` (Docker mode) with `MM_CRUD_ROWS=10000` and record the new `cols/row`, `WAL B/row`, and `unidb÷PG` ratios in `PROGRESS.md`.

## Acceptance criteria

- After Step 1: `cols/row` for DELETE selected drops below 2.0 (from 6.00).
- After Step 1: DELETE selected reaches ≥ 0.28× PG at 10k rows (from 0.16×).
- After Step 2: `cols/row` for UPDATE drops below 5.0 (from 8.00).
- After Step 2: UPDATE bulk reaches ≥ 0.17× PG at 5k rows (from 0.14×).
- All existing UPDATE/DELETE correctness tests pass; crash harness green.
- No regression on SELECT cols/row (B2 SELECT path must remain unchanged).

**Correction note (2026-07-16, post-measurement):** The criteria above were based on a wrong
model of the old code. `matching_rows` already called `deform_row` (predicate-only decode)
for non-matching rows before this item shipped — they were never paying the full-column
decode cost. Consequently:

- **DELETE Step 1 result:** `cols/row` 6.00 → **2.00** ✓ (theoretical floor at 50% selectivity:
  scan visits 2N rows, N deleted, 1 pred col each → 2N÷N = 2.0; criterion `< 2.0` should
  read `≤ 2.0`). `dec/row` 1.00 → **0.00** (full decodes eliminated). Throughput: 614k →
  675k rec/s (+10%), ratio holds at **0.16× PG** — the real bottleneck is WAL xmax-stamp
  writes (114 B/row), not column decoding. The `≥ 0.28× PG` criterion was wrong.
- **UPDATE Step 2 result:** `cols/row` **8.00 → 8.00** (no change). The old code's
  `deform_row` for non-matching rows already cost only 1 col each; the full decode of
  matched rows is unavoidable (needed to compute new values). The `MatchedRows` type
  change (raw bytes instead of `Vec<Literal>`) is architecturally sound and required by
  the DELETE win, but produces no measurable change to UPDATE's cols/row metric.
  The `< 5.0` criterion and `≥ 0.17×` throughput criterion are not achievable via this
  approach — beating UPDATE cols/row requires Phase C (HOT chain, locked decision D4).

## Depends on / builds on

- `src/sql/executor.rs` — `exec_update`, `exec_delete`, `matching_rows`.
- `src/heap.rs` — `deform_row(pred_upto, pred_needed)` already implemented for the SELECT B2 path.
- Item 47 Phase A (`47_update_delete_write_throughput.md`) — SHIPPED (PR #119). Phase B is the next logical step in the same file's phased plan.
- Item 43 (`43_a3_gate_size_aware_selectivity.md`) — SHIPPED. A3 gate routes selective UPDATE/DELETE to the scan path where this fix applies.

## What this does NOT fix

- UPDATE's write-step decode (all 8 columns) — required by insert-new-version MVCC.
- The WAL B/row=528 for UPDATE — unchanged by Phase B; addressing it needs Phase C (HOT chain, locked decision D4).
- DELETE all throughput — already covered by the TRUNCATE fast path (item 48).
