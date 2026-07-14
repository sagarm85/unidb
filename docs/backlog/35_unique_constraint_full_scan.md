# Item 35 — Unique-constraint enforcement is a full heap scan (O(n²) INSERT/UPDATE)

**Type:** Improvement
**Status:** NOT STARTED
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

## Proposed fix (open — the implementing session should re-derive the exact
mechanism per CLAUDE.md §0.6.2/§0.6.3 before committing to one)

Back `PRIMARY KEY`/`UNIQUE` columns with a **real index-backed uniqueness
check** (a point lookup) instead of a full heap scan, reusing the durable
B-tree/index machinery already used for secondary `CREATE INDEX ... USING
BTREE`. Open design questions to resolve first, not to assume:

1. **Where does the index come from?** Options: (a) auto-create an implicit
   durable B-tree index for every `PRIMARY KEY`/`UNIQUE` column set at
   `CREATE TABLE` time (a catalog/schema change — check interaction with item
   25's multi-page catalog chain, and whether this needs a `FORMAT_VERSION`
   bump / §3 sign-off), or (b) reuse an existing secondary index when one
   already covers the column, falling back to the heap scan only when no
   index exists (smaller change, but leaves the common case — a freshly
   created table with no explicit `CREATE INDEX` — unfixed unless combined
   with (a)).
2. **Multi-row-INSERT-visibility interaction.** `enforce_unique`'s own
   comment notes it scans under a per-row snapshot so duplicates *within the
   same statement* (own-xid writes) are caught — confirm an index-based
   check preserves this (i.e. the index must see uncommitted-but-own-xid
   inserts from earlier in the same batch, or an equivalent check is added).
3. **Recovery/crash-safety** if an implicit index is introduced: it needs
   the same crash-recovery coverage every other durable index gets (redo-only
   `WAL_INDEX`, a crash-harness point) — do not assume it's free.

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
