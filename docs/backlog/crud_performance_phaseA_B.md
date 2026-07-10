# CRUD performance — Phase A (write path) + Phase B (scan/read path)

## Status as of 2026-07-10: **NOT STARTED** (planned)

Core-lane performance work. Closes the CRUD-stress gap surfaced by the
multi-model report (`benches/decompose.rs`, Table 3 + Table 3.1) against a
matched-durability Postgres 18.4 baseline. Two phases, each a separate PR;
Phase A first (biggest ROI), Phase B second. No §3 (locked-decision) reversal;
crash harness must stay green (Phase A touches the write + index path).

---

## 0. The evidence (why this exists)

Table 3 (20,000-row table, `k` btree-indexed, unidb `F_FULLFSYNC` vs Postgres
`fsync_writethrough` — matched durability). Every op is wrapped in **one**
`begin()…commit()` (`decompose.rs:1700`), so there is **one fsync per
operation, not per row** — the per-row cost below is pure CPU + WAL volume, not
durability:

| operation                | records | unidb rec/s | pg rec/s  | unidb÷pg | per-row (unidb) |
| ------------------------ | ------- | ----------- | --------- | -------- | --------------- |
| INSERT (bulk)            | 20000   | 293         | 298       | 0.98×    | fsync-bound ✅   |
| SELECT filtered (k<N)    | 20000   | 312,924     | 2,077,868 | 0.15×    | 3.2 µs          |
| SELECT grouped (GROUP BY)| 40000   | 4,408,888   | 5,585,389 | 0.79×    | 0.23 µs         |
| UPDATE bulk (k<N/2)      | 10000   | 34,870      | 305,412   | **0.11×**| **28 µs** ⛔     |
| DELETE selected (k>=N)   | 20000   | 272,948     | 1,389,830 | 0.20×    | 3.7 µs          |
| DELETE all               | 20000   | 290,254     | 1,088,485 | 0.27×    | 3.4 µs          |

Table 3.1 full-scan `SELECT COUNT(*)`: unidb 6.4M/s vs PG 50M/s at 1M rows
(~8×), 6.5M/s vs 38M/s at 2M rows.

INSERT is at parity and durability-matched — **out of scope, do not touch it.**

---

## 1. Root causes (each traced to a line)

- **RC1 — UPDATE/DELETE always full-scan the heap; the index is never used.**
  `exec_update`/`exec_delete` both call `matching_rows` (`src/sql/executor.rs:1109`),
  which is `heap.scan(...)` → decode *every* row → `predicate_matches` in memory.
  The `k` btree is consulted **only** by SELECT (`try_exec_select_btree`,
  `executor.rs:825`), never by the write paths. `DELETE … WHERE k >= lo` decodes
  all 20k rows to find its matches.

- **RC2 — UPDATE re-indexes unchanged columns, one full-page-image WAL record
  per row. This is the 0.11× / 28 µs killer.** `apply_durable_index_writes`
  (`executor.rs:178`) unconditionally re-inserts *every* indexed column of *every*
  updated row into its `DiskBTree`, with **no changed-value check**. The bench
  updates `body` while `k` is unchanged, yet the `k` btree is rewritten 10,000
  times, each `DiskBTree::insert` emitting a full 8 KiB `WAL_INDEX` page image
  (~80 MB WAL for a column nobody touched). MEMORY.md already fingered
  "`WAL_INDEX` full-page-image append" as the residual concurrency bottleneck —
  same mechanism, now dominating single-thread UPDATE.

- **RC3 — Row-at-a-time interpreted execution: per-row allocation + per-row
  re-snapshot.** `matching_rows` materializes a `Vec<(RowId, Vec<Literal>)>` for
  the whole table (a heap-allocated `Vec<Literal>` + a `String` per TEXT per
  row), then filters. `exec_update` additionally takes a **fresh
  `snapshot_for_statement` inside the per-row loop** (`executor.rs:1012`) even
  when there is no UNIQUE constraint.

- **RC4 — Full-scan `COUNT(*)` fully decodes + visibility-checks every row into
  `Literal`s it discards.** `COUNT(*)` should count visible slots and decode
  zero columns; a projection should decode only referenced columns. Today every
  path goes through the whole-row decode.

- **RC5 (latent, not hit by this bench — fix opportunistically).**
  `enforce_unique` (`executor.rs:1561`) does a **full heap scan per row** →
  O(N²) the moment a table has a UNIQUE constraint. Phase A #2's index path
  should serve it.

---

## Phase A — write path (UPDATE / DELETE). Target: UPDATE 0.11× → ~parity, selective DELETE 0.20× → ~parity.

One PR. Ordered checkpoints A1 → A4. Crash harness must stay green.

### A1 — Skip index maintenance for unchanged indexed columns (fixes RC2; the single biggest win)

**Change.** In `exec_update` (`executor.rs:983`), before calling
`apply_durable_index_writes`, compute which indexed columns actually changed
between the pre-image `row` and the post-image `coerced`. Add a variant —
`apply_durable_index_writes_changed(table_def, new_row_id, old_row_id, old, new, ctx)`
— that, per indexed column:
  - if `old[idx] == new[idx]` (by coerced `Literal` equality): **skip entirely**
    — no btree insert, no `WAL_INDEX` record. The existing btree entry still
    points at a valid version; MVCC re-validation + vacuum already handle the
    superseded RowId (this is the invariant P3.a relies on — an index entry is a
    hint, `try_exec_select_btree` re-checks visibility).
  - if changed: insert the new value (as today) **and** schedule removal of the
    stale `old_value → old_row_id` entry via the vacuum/`remove` path already
    used by M10 (`DiskBTree::remove`), or leave it for vacuum if that keeps A1
    small — document which.

**Expected.** WAL volume for a body-only UPDATE collapses from ~80 MB to ~0 index
bytes; UPDATE moves from 0.11× toward parity. This is the highest-ROI change in
either phase — land and measure it first, alone, so its contribution is isolated.

**Correctness note.** Equality must be over the **coerced** values (post
`coerce_and_validate_row`), matching how the value was originally indexed, so a
no-op rescale (`9.9` vs `9.90`) is correctly seen as unchanged.

### A2 — True same-page (HOT-style) update when no indexed column changed

**Change.** When A1 determines *no* indexed column changed AND the new encoded
tuple fits the current page's free space, perform an in-place / same-page new
version so no secondary index needs touching at all (Postgres HOT). If it does
not fit, fall back to the normal `heap.update` (new page + the A1-filtered index
writes). Gate behind the existing heap update machinery; do **not** invent a new
WAL record if the current `heap.update` path can express it — reuse
`WAL_INSERT`/xmax-stamp bracketing (D2).

**Expected.** Removes the remaining new-version churn for the common
"update a non-indexed column" case; compounds A1.

**If A2 proves fiddly against the MVCC version model, ship A1 alone and file A2
as a follow-up** — A1 is where most of the win is.

### A3 — Index-driven UPDATE/DELETE (fixes RC1)

**Change.** Give `matching_rows` (`executor.rs:1109`) the same sargable-index
range path SELECT already has. When the predicate is a range/equality on an
indexed column, drive row lookup from `DiskBTree` range scan (reuse
`try_exec_select_btree`'s descent) instead of `heap.scan`. Keep the full-scan
fallback for non-sargable predicates (always correct).

**Expected.** Large win for *selective* DELETE/UPDATE (few matches out of many
rows); for "matches most rows" cases it at least removes the whole-table
materialize. Also removes RC5's O(N²) unique-scan exposure for indexed columns.

### A4 — De-loop the per-row overhead (fixes RC3)

**Change.** In `exec_update`: hoist the `snapshot_for_statement` out of the
per-row loop (currently `executor.rs:1012`); when `unique_column_sets` is empty
skip the unique machinery entirely (it early-returns today but still allocates a
snapshot per row). Avoid materializing the full `Vec<(RowId, Vec<Literal>)>`
where an iterator suffices.

**Acceptance (Phase A).** `benches/decompose.rs` Table 3 re-run, native, matched
durability:
- UPDATE bulk: **0.11× → target ≥ 0.8×** (WAL-bytes-per-UPDATE reported
  before/after — the proof, per §6/C1).
- DELETE selected: **0.20× → target ≥ 0.8×**.
- No regression on INSERT (0.98×) or SELECT.
- Crash harness green (A1/A2/A3 touch write + index; add a crash point if the
  update-version/index-skip interaction isn't covered by existing P13–P17).

---

## Phase B — scan / read path (SELECT, COUNT). Target: filtered SELECT 0.15× → ~0.5–0.7×, COUNT scan 8× → ~2×.

One PR, after Phase A. No write-path or crash-recovery change → harness count
unchanged. Ordered B1 → B3.

### B1 — Decode pushdown: COUNT(*) counts visible slots, decodes nothing (fixes RC4)

**Change.** Add a scan mode that returns visible-row *count* (or RowIds) without
decoding column bytes. Route `SELECT COUNT(*)` and any aggregate that needs no
column values through it. Today the grouped/count paths go through the same
whole-row decode as a projection.

**Expected.** COUNT(*) full scan 6.4M/s → materially closer to PG's slot-count
loop; grouped (0.79×) improves as a side effect.

### B2 — Projection pushdown: decode only referenced columns (fixes RC3/RC4 for SELECT)

**Change.** `matching_rows` and the SELECT scan decode the **whole** row into
`Vec<Literal>` (`executor.rs:1118`) regardless of projection. Decode only the
columns referenced by the projection + predicate. Prefer a lazy row-view over
the page bytes (`decode_row` → per-column offset walk that stops early / skips
unreferenced columns) over allocating a full `Vec<Literal>`.

**Expected.** Filtered SELECT (0.15×) — the `SELECT id, body …` bench projects 2
of 4 columns; decoding only those + the predicate column removes per-row
allocation and String churn. Target 0.15× → ~0.5×.

### B3 — Streaming operators (fixes RC3 structurally)

**Change.** Stop materializing `Vec<Vec<Literal>>` per operator in the query
executor (`query_exec.rs` batches). Iterate/stream where the operator allows.
Larger refactor — do it **after** B1/B2 prove the decode cost, and only if the
numbers justify it.

**Acceptance (Phase B).** Table 3 filtered SELECT ≥ 0.5×; Table 3.1 COUNT scan
gap ≤ ~2× at 1M/2M. Differential-correctness vs SQLite (existing harness)
unchanged. Peak RSS unchanged.

---

## Phase C — measurement discipline (do this first, alongside A1)

Per CLAUDE.md §6: every fix proven with a number, never asserted.

- **C1** — instrument `benches/decompose.rs` to report **WAL bytes written per
  operation** (expose a `Wal` byte counter or diff `wal_bytes`) and **rows
  decoded per operation**. A1's win is invisible in rec/s noise but obvious in
  WAL-bytes; make it measurable before changing code.
- Record before/after tables in `PROGRESS.md` (one entry per phase) and flip
  this doc's status line to SHIPPED with a pointer, per §9.

---

## Honest expectations / non-goals

- A1 + A3 (write path) and B1 + B2 (decode pushdown) are where the money is.
- The **"return all 20k rows"** filtered case and the **raw sequential COUNT
  scan** will stay somewhat behind Postgres's tight C scan loop — that is the
  CLAUDE.md §1 reality ("we rebuild Postgres, we don't beat it single-model").
  6–8× is not that gap; it is removable waste. Target parity-ish on the write
  path and ≤2× on pure scans; do **not** chase sub-1× on single-model scans.
- Do not touch INSERT (fsync-bound, at parity).
- No §3 decision reopened. D5 (WAL-before-page), D2 (mini-txn bracketing), D1
  (redo+undo) all hold. A1's index-skip relies on the *existing* P3.a invariant
  (index entry is a re-validated hint), not a new one.

## Sequencing

```
C1 (instrument) ─┐
                 ├─ A1 (measure in isolation) → A2 → A3 → A4 → Phase A PR + PROGRESS entry
                 └─ then B1 → B2 → (B3 if justified) → Phase B PR + PROGRESS entry
```

A1 alone, measured, is the recommended first commit — it is the largest single
win and the cheapest to land.
