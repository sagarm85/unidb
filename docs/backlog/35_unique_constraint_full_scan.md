# Item 35 — Unique-constraint enforcement is a full heap scan (O(n²) INSERT/UPDATE)

**Type:** Improvement
**Status:** SHIPPED 2026-07-14 — see `PROGRESS.md` (item 35)
**Priority:** Critical — hits nearly every real schema (any table with a `PRIMARY KEY`), not a niche path

---

## Problem

Found via a `unidb-studio` demo run: `demo/seed.py` seeding 20,000 `customers`
rows through the item-32 `POST /tables/{name}/bulk` endpoint showed severely
**degrading** throughput — 2,180 → 1,106 → 735 rows/s as the batch progressed,
not the flat rate a fixed per-call overhead would produce.

Confirmed empirically in-engine with a controlled experiment (same bulk
endpoint, same batch sizes, the only variable is a `PRIMARY KEY`):

| Table | 5k rows | +5k (10k cume) | +5k (15k cume) |
|---|---:|---:|---:|
| `id INTEGER PRIMARY KEY` | 4,955 rows/s | **1,685 rows/s** | **1,013 rows/s** |
| `id INT` (no PK) | 119,047 rows/s | 113,636 rows/s | 113,636 rows/s |

Same schema and data, **>100× slower with a `PRIMARY KEY`**, and degrading
further with every chunk while the no-PK case stays perfectly flat. This matches
the demo's observed curve closely (both fit `time ≈ c·N²`).

## Root cause

`enforce_unique()` (`src/sql/executor.rs:2145`) enforces every `PRIMARY
KEY`/`UNIQUE` constraint by `heap.scan()`-ing and decoding **every
currently-visible row in the table**, once per inserted/updated row:

```rust
for (row_id, bytes) in heap.scan(snapshot, xid, pool)? {   // scans EVERY existing row
    let existing = decode_row(&bytes, &table_def.columns)?;
    if set.iter().all(|&i| existing[i] == new_row[i]) { ... }  // linear compare
}
```

`PRIMARY KEY` gets **no backing index** in this engine —
`unique_column_sets()` (`:2116`) treats `col.constraints.primary_key` and
`table_def.constraints.primary_key` identically to `unique`; both are
heap-scan-only, never index-assisted.

- **INSERT** (`exec_insert`, call site `~:817`) calls `enforce_unique`
  **unconditionally** whenever the table has any unique set — no gate at all.
- **UPDATE** has a `has_unique` early-exit (`:1440`, the A4 optimization,
  `:1457` the guarded call site) — but it only skips the call when a table has
  **no** unique constraint whatsoever. Once `has_unique` is true (any
  PK/UNIQUE column — i.e. almost every real table), UPDATE pays the identical
  O(n) scan per row. **A4 never fixed the O(n) nature of the check itself; it
  only skipped it in the one case where it was already a no-op.**

Net effect: inserting N rows into a PK'd table costs **O(N²)** — every demo
table in `unidb-studio` (`customers`, `products`, `orders`, `order_items`,
`invoices`, `invoice_items`) declares `id INTEGER PRIMARY KEY`, so all of them
are affected, and it gets catastrophically worse past the ~15–20k rows shown
here (a 100k-row load would be roughly 40× slower *per row* than the 15k
point already measured — minutes-to-hours instead of seconds).

**Both INSERT and UPDATE are affected — this is not a bulk-load-specific bug.**
Item 32's own throughput benchmark (~12k–31k rows/sec, `PROGRESS.md`) never
caught this because its test tables (`bt_noidx`, `bt_idx`) had **no
`PRIMARY KEY` at all** — `enforce_unique` was a guaranteed no-op there. The
bulk endpoint itself is not at fault; `Engine::bulk_insert` reuses the
ordinary `execute_prepared(INSERT...)` path, so it inherited a pre-existing,
previously-uncovered gap shared by plain `/sql` INSERT and UPDATE too.

## Fix plan (phased)

The mechanism is the standard one: back `PRIMARY KEY`/`UNIQUE` columns with a
**real index-backed uniqueness check** (a point lookup) instead of a full heap
scan, reusing the durable B-tree machinery already used for secondary
`CREATE INDEX ... USING BTREE` (`DiskBTree`). Per CLAUDE.md §0.6.2 the
implementing session must re-derive the exact mechanism against *this* engine's
insert-new-version MVCC before committing — but the phasing and the invariants
below are not optional, they are what makes the difference between a fast-and-
correct fix and a fast-and-silently-corrupt one.

### Phase 0 — Capture the "before" (do this first, no code change)
Establish the baseline the fix is measured against (see **Before/after
measurement plan** below): run the PK-vs-no-PK micro-benchmark and regenerate
the multi-model at-scale report **with a `PRIMARY KEY` on the relational
table**. Note that the existing report (`multi_model_report_*.md`, Table 3.1)
loads a *no-PK* table, which is exactly why it never surfaced this bug — fixing
that blind spot is part of this item.

### Phase 1 — Index-backed uniqueness check
Replace the `heap.scan()` in `enforce_unique()` with a point lookup into a
B-tree keyed on the constraint column(s). Open decision to resolve, not assume:
where the index comes from — (a) auto-create an **implicit** durable B-tree for
every `PRIMARY KEY`/`UNIQUE` column at `CREATE TABLE` time (a catalog/schema
change — check interaction with item 25's multi-page catalog, and whether it
needs a `FORMAT_VERSION` bump / §3 sign-off), or (b) reuse an existing secondary
index when one covers the column and fall back to the scan otherwise (smaller,
but leaves the common freshly-created-table case unfixed unless combined with
(a)). (a) is the real fix; (b) alone is a half-measure.

**Composite keys are FUTURE scope, not this item.** Do **not** build or require
composite-key support here. Do make one cheap forward-compatibility choice: make
the index key an *encoded value/tuple*, not a hardcoded single scalar, so that
when composite `PRIMARY KEY`/`UNIQUE` lands later it is a key-encoding extension,
not a rewrite of the check. That is the only composite concern in scope.

### Phase 2 — Correctness invariants the fix MUST preserve
These are the ways an index-backed uniqueness check silently corrupts data if
done naively. Each needs a test, not an assumption:

1. **MVCC visibility — not "key exists → reject".** unidb does insert-new-
   version MVCC: an `UPDATE` to a unique column leaves the old version's key in
   the index until vacuum, so one key legitimately maps to multiple RowIds. The
   check must be *"a **visible-or-in-flight** row with this key exists (excluding
   my own current version) → reject"*. Get the visibility predicate wrong and
   you either reject valid inserts (stale dead entry read as live) or **allow
   duplicates** (live entry read as dead). This rides the exact visibility logic
   item 16 broke and fixed — treat it as the load-bearing risk.
2. **Own-xid / same-batch duplicates.** Two rows with the same key in one multi-
   row INSERT or bulk batch must be caught before commit (the index/check must
   see the writer's own uncommitted inserts). This is the case the bulk endpoint
   hits hardest and the easiest to silently break.
3. **Concurrent inserters racing the same key** (single-primary, concurrent
   writers, item 11): first-committer-wins abort or a lock — must be race-safe
   under the concurrency matrix (`benches/conc_matrix.rs`), not just single-
   threaded.
4. **NULL distinctness.** Multiple NULLs do not violate `UNIQUE`; the current
   code skips any set with a NULL component — preserve exactly (don't index NULL
   keys, or skip the check when the key has one).
5. **Recovery/crash-safety** if an implicit index is added: same coverage every
   durable index gets — redo-only `WAL_INDEX`, O(1) reopen (never rebuilt on
   open), a new crash-harness point. Do not assume it's free.

### Phase 3 — Measure the "after" and regenerate the report
Re-run everything from Phase 0 and confirm the PK curve is now flat and the
multi-model report's PK'd insert row scales like the no-PK one (see below).

## Before/after measurement plan

Two layers of evidence, both captured in Phase 0 (before) and Phase 3 (after).
Numbers below are the **measured "before"**; the "after" column is the target
the fix must hit, filled in when it lands.

### Layer 1 — Micro-benchmark (the direct repro, becomes a permanent regression test)

Two tables, identical schema except one has `id INTEGER PRIMARY KEY`; three
5,000-row bulk chunks each via `POST /tables/{name}/bulk`, timing each chunk.
The signal is **shape**: PK'd throughput must stop degrading and go flat.

| Table | 5k rows | +5k (10k) | +5k (15k) | shape |
|---|---:|---:|---:|:--|
| `id INTEGER PRIMARY KEY` — **before** | 4,955/s | 1,685/s | 1,013/s | O(n²), degrading |
| `id INTEGER PRIMARY KEY` — **after (target)** | ~113k/s | ~113k/s | ~113k/s | flat |
| `id INT` (no PK, reference) | 119,047/s | 113,636/s | 113,636/s | flat |

Same test for **UPDATE** on a PK'd/UNIQUE'd table (Phase 2 covers the UPDATE
path too, not just INSERT), and a **100k+** run that must finish in the same
order of magnitude as the no-PK case.

### Layer 2 — Multi-model at-scale report (regenerate in the existing format)

Regenerate `docs/performance/multi_model_report_<ts>.md`
(`scripts/report.sh`), but with a **`PRIMARY KEY` on the relational table** so
the report actually exercises `enforce_unique` — closing the blind spot that let
this ship. The load-bearing table is **3.1 (bulk insert scaling)**; the fix must
turn its PK'd insert row from size-dependent to flat, and must **not** regress
the multi-model commit-cost tables (1 and 4).

**Table 3.1 — bulk insert scaling, PK'd table (the row that exposes the bug):**

| rows | unidb insert **before** (rec/s) | unidb insert **after** (target) | postgres insert (rec/s) |
|-----:|-------------------------------:|-------------------------------:|------------------------:|
| 10,000 | (degrading — fill from Phase 0) | flat, ~engine batched ceiling | ~35,823 |
| 1,000,000 | (falls off a cliff) | same order as no-PK | ~37,755 |
| 2,000,000 | (falls off a cliff) | same order as no-PK | ~37,961 |

**Table 1 — multi-model commit cost (must be unchanged by the fix):** W0–W4
ms/commit at each size — the fix touches the uniqueness check, not the four-model
commit path, so `W4/W0` must stay ~1.1–1.2×, confirming no regression to the
actual competitive thesis (the atomic cross-model commit).

**Table 4 — unidb multi-model (1 txn) vs Postgres relational:** `unidb ÷ PG`
must stay in its existing band (~0.87–0.91×) — the fix must not move it.

Interpretation guardrail (§6): the win here is **removing an O(n²) penalty**, not
beating Postgres on single-table INSERT — Table 3.1's honest target is "flat and
within the same order of magnitude as no-PK / as PG," not a headline PG win.

## Acceptance criteria

- [ ] The PK-vs-no-PK scaling comparison above becomes a **permanent
      regression test**: inserting N rows into a `PRIMARY KEY` table shows
      **flat** rows/sec regardless of N (no degradation as the table grows),
      for both the bulk endpoint and plain `/sql` INSERT.
- [ ] Same flat-throughput proof for **UPDATE** on a PK'd/UNIQUE'd table.
- [ ] A large-N run (100k+ rows) with a `PRIMARY KEY` completes in the same
      order of magnitude as the no-PK case (not 40×+ slower).
- [ ] Existing uniqueness/duplicate-rejection correctness tests still pass
      unchanged — the fix must not weaken the constraint, only speed up its
      enforcement. Include a same-statement-duplicate test (two rows with the
      same PK value in one multi-row INSERT/bulk batch) to lock in point 2
      above.
- [ ] Crash harness unaffected — unless an implicit index is added, in which
      case: a new crash point proving recovery of the implicit index, and an
      explicit §3 sign-off recorded in `PROGRESS.md` if a `FORMAT_VERSION`
      bump is genuinely required (do not assume one is needed without
      checking first).
- [ ] `docs/REST_API.md` / `engine_access_guide.md` updated if the fix changes
      any observable behavior (e.g. `CREATE TABLE` now implicitly creates an
      index visible in `unidb_catalog.indexes`).
- [ ] **Before/after evidence recorded in `PROGRESS.md`:** both layers of the
      measurement plan filled in with real numbers — the micro-benchmark table
      (PK before→after→no-PK) and a **regenerated multi-model report** with a
      `PRIMARY KEY` on the relational table, showing Table 3.1's PK'd insert row
      now flat and Tables 1 & 4 unchanged (`W4/W0` ~1.1–1.2×, `unidb ÷ PG`
      ~0.87–0.91×). The regenerated report replaces the no-PK blind spot that let
      this ship.
- [ ] **Composite keys remain out of scope**, but the index-key encoding is
      tuple-ready (documented in the PR) so future composite support is an
      extension, not a rewrite.

## Evidence / repro

Reproduced with a temporary integration test (not committed) using the
existing `tests/server_bulk.rs`-style harness: two tables, identical schema
except one has `id INTEGER PRIMARY KEY`, three 5,000-row bulk-insert chunks
each via `POST /tables/{name}/bulk`, timing each chunk. The PK table's
per-chunk rate fell 4,955 → 1,685 → 1,013 rows/s; the no-PK table stayed flat
at ~113k–119k rows/s. The demo script that surfaced this:
`unidb-studio/demo/seed.py` → `bulk_insert("customers", rows)` →
`POST /tables/customers/bulk` → `Engine::bulk_insert` →
`execute_prepared(INSERT...)` → `enforce_unique()`.

## Depends on / builds on

- Item 32 (bulk-load API) — the endpoint that surfaced this; not itself the
  cause.
- Item 25 (multi-page catalog) — relevant if the fix needs a new implicit
  per-table index entry in the catalog.
- The existing durable B-tree index machinery (`DiskBTree`,
  `CREATE INDEX ... USING BTREE`) — the mechanism to reuse, not rebuild.
