# Durable free-space map + O(1) table-page representation

## Status as of 2026-07-10: **SHIPPED** (Core lane, branch `durable-fsm`; one PR, commits B1 → B2 → B-accept + docs). The `HeapFull` ceiling is removed; page directory + free-space map are a per-table durable `DiskBTree`. Metrics + verdict in `PROGRESS.md`'s "Durable on-disk FSM + catalog page-list" entry. See "Implementation (B1 / B2)" and "B-accept" below.

Filed from the Postgres baseline comparison (`pg_baseline_comparison.md`, PR #25),
which root-caused a hard scaling limit in the SQL insert path. This is the fix.
Not part of that benches-only lane — it touches `src/` (catalog, heap, recovery)
and is its own checkpoint.

## Problem — the evidence

Building a table past **~145k rows via the SQL `INSERT` path** fails with
`HeapFull { size: 8138 }`. The raw CRUD path (`Engine::insert`) builds **5M rows
linearly** (proven, ~247 s) — so it is *not* a heap/data limit.

Root cause (verified in source, not the "lazy FSM" the first pass guessed):

- The catalog is persisted as a **single serialized JSON blob**
  (`src/catalog.rs`: "The catalog is persisted as a single serialized blob").
- `TableDef.pages` is an **unbounded `Vec<PageId>` — one entry per heap page the
  table owns** (`src/catalog.rs:241`).
- The SQL insert path rebuilds a `Heap::from_pages(table_def.pages.clone())` per
  statement and, whenever it allocates a new page, calls
  `persist_pages_if_changed` → `Catalog::set_pages`, which **rewrites the whole
  page list into the catalog blob** (`src/sql/executor.rs:482`, `:496`).
- That blob is stored as a **single tuple that must fit one 8 KiB page**. At
  ~1,450 heap pages (~145k tiny rows) the encoded page list alone approaches the
  ~8,138-byte usable page space, so the next catalog write fails. The reported
  `size: 8138` is the **catalog blob**, not a data row.

The raw path holds one long-lived in-memory `Heap` and **never rewrites the
catalog**, so it is immune. Two distinct issues are tangled here:

1. **(Hard cap)** the O(heap-pages) page-list blob overflows a page → the ~145k
   ceiling. *This is the bug.*
2. **(Perf)** the per-statement `Heap::from_pages` rebuild re-probes pages with a
   cold FSM every statement. *Real cost, but only below the cap.*

## Design constraint that pins the solution

The project's moat is **O(1) `Engine::open`** — no heap/index rescans on open
(P3.a–P3.c made every secondary index durable precisely for this). Any fix that
reconstructs per-table free-space by *scanning* pages at open (e.g. walking an
on-disk linked page list) reintroduces O(pages) open cost and **breaks the moat**.
Therefore the free-space structure must be **durable and crash-recovered**, using
the same pattern the durable indexes already use: structure lives in the shared
page store, WAL-logged as full-page images (`WAL_INDEX`-style), never rebuilt.
This is why the fix is a checkpoint, not a one-line patch.

## Options considered

- **A — cache the `Heap`, defer the catalog write (interim only).** Keep a
  long-lived per-table `Heap` on the `Engine` (as `edge_index_meta` is cached) so
  the SQL path shares the warm in-memory FSM and stops rebuilding per statement;
  flush the page-list delta at checkpoint/txn boundary instead of per insert.
  **Fixes issue 2 (perf), NOT issue 1 (the cap)** — the blob is still O(pages) and
  still overflows, just at the next checkpoint instead of mid-insert. A stopgap,
  not the fix. Reach for it only if SQL bulk-load past ~145k is needed *before*
  the real work lands, and document that it does not raise the ceiling.

- **B — durable FSM + O(1) table-page representation (the fix).** Two parts:
  - **B1. Replace `TableDef.pages: Vec<PageId>` with an O(1) representation.**
    Either **extents** — a small `Vec<(start, len)>` of page ranges (InnoDB/most
    engines; collapses 1,450 ids to a handful) — or a **head-page id + `next_page`
    links** in the page header (catalog stores O(1); the list lives in the pages).
    Extents are preferred: they keep random access and stay tiny.
  - **B2. A durable free-space map** as its own structure in the shared page
    store (a bitmap or small tree of *pages-with-space*, WAL-logged full-page
    images, crash-recovered — mirrors `DiskBTree`). Removes the per-statement
    rebuild *and* the cold-FSM probe, and preserves O(1) open (the FSM is read,
    never recomputed). Postgres's `_fsm` fork is the reference shape.

  **B subsumes A**: with a durable FSM and an O(1) page representation there is no
  growing page-list blob to rewrite and no per-statement rebuild — A's benefit
  falls out for free.

## Recommendation

Ship **B** as one Core-lane checkpoint. Do **not** ship A as a permanent
solution (it leaves the ceiling in place). Sequence inside B: **B1 first**
(removes the hard cap immediately, small blast radius — a catalog field + its
migration + `set_pages`/`from_pages` call sites), then **B2** (the durable FSM,
larger — new page type / WAL usage / a crash point, but the same proven pattern
as P3.a).

## Checkpoints (proposed)

- **F1 — extent-based `TableDef` page representation.** Change `pages: Vec<PageId>`
  → `extents: Vec<(PageId, u32)>` (or add head-page + `next_page`); update
  `Heap::from_pages`/`page_ids`/`set_pages`/`persist_pages_if_changed` and every
  `from_pages` call site. Forward-compatible catalog migration (serde default /
  format-version bump per D9 if needed). **Removes the ~145k ceiling** — regression
  test: build 1M rows via the SQL path.
- **F2 — durable free-space map.** New durable FSM structure (page-store-resident,
  WAL-logged, crash-recovered); `Heap` reads it instead of rebuilding per
  statement; vacuum updates it. New crash point (kill mid-FSM-update → reopen →
  free space correct, O(1) open preserved). Benchmark: SQL bulk-load throughput
  now matches the raw path; open time flat vs table size.

## Implementation (B1 / B2) — as built

The two checkpoints are implemented on the existing **`DiskBTree`** rather than a
bespoke extent/bitmap structure, because the spike (P3.a–P3.d) already made
`DiskBTree` durable, WAL-logged (`WAL_INDEX` full-page images), crash-recovered,
and O(1)-open — exactly the properties the "design constraint that pins the
solution" section demands, for free. **The per-table FSM is a `DiskBTree` keyed
`page_id → free_bytes`; its keys are the page directory, so it also subsumes F1's
page representation.** `TableDef` stores one stable `fsm_meta: Option<PageId>`
(like `ColumnDef.index_root` / the edge & LOB index meta pages), minted at
`create_table`. **No data-dir migration:** `pages` is retained as a
`#[serde(default)]` legacy fallback (a pre-FSM catalog with `fsm_meta == None`
still opens and scans via it), and no `FORMAT_VERSION` bump is needed (additive,
D9-consistent).

- **B1 (page directory off the catalog blob).** `TableDef.fsm_meta` replaces the
  growing `pages` blob; `Heap::open(fsm_meta, …)` is O(1) (no directory load); the
  insert path finds the append tail via `DiskBTree::max_entry` (O(log n), not the
  O(pages) walk); a full `scan`/vacuum lazily loads the directory via
  `DiskBTree::page_directory` over any `PageReader` (buffer pool **or** the
  concurrent-read mmap); `persist_pages_if_changed`/`set_pages` are no-ops for
  FSM-backed tables. **Removes the ~1,450-page (`HeapFull`) ceiling** — regression
  test builds a table past it via the SQL path.
- **B2 (durable free-space + crash safety).** Free-space values go durable in the
  FSM tree (removing the per-statement cold-FSM rebuild); vacuum's
  `compact_page`/reclamation updates it; alloc+FSM-insert become one atomic
  mini-txn; new crash points (mid-FSM-update, mid-heap-grow).

## B-accept — validate against the benchmark that found the bug

This milestone exists because running `pg_baseline_comparison.md`'s size sweep
(`benches/decompose.rs` + `scripts/pg_compare.sh`) at scale hit `HeapFull`. B-accept
re-runs that exact benchmark, before vs after, and records results in `PROGRESS.md`.
**This gate can legitimately fail** — if a re-run still errors or shows no real
improvement, the milestone is not done; report the finding, do not rationalize it.

1. **Correctness (primary pass/fail).** The size-sweep / SQL-bulk-load run that
   previously died with `HeapFull { size: 8138 }` now completes cleanly at the
   scale that exposed it (identify the config from the pg-baseline `PROGRESS.md`
   entry + the "correct HeapFull root cause" commit).
2. **Improvement — throughput/open.** Catalog-write cost, cold-open, and insert
   throughput at scale, before vs after (the durable FSM should flatten the
   O(pages) catalog rewrite).
3. **Improvement — concurrent SQL writes (refinement, 2026-07-10).** Re-run
   `pg_baseline_comparison.md`'s **B3 concurrent-SQL-write comparison** — N unidb
   writer threads on the SQL path vs N Postgres connections, N ∈ {1,2,4,8}, matched
   durability — **before vs after**, and record the scaling curve. Rationale: today
   the SQL insert path takes the catalog `RwLock` write-lock at
   `sql/executor.rs` `set_pages` (persisting the grown page-list) on every heap
   growth; moving the page-list into the per-table durable FSM should reduce that
   catalog-write contention and improve concurrent-SQL-write scaling. **Measure,
   don't assume:** if the curve improves, record by how much; if it does not (or
   barely moves), that is a real finding — report the next serialization point
   (FSM page-latch contention, index-page latches, executor locking) rather than
   claiming it is fixed.

## Non-goals / notes

- Stays within **D6** (single shared file) — extents and durable FSM both live in
  the one file; no multi-file split.
- Not a full `pg_class`/`pg_attribute` system-catalog rewrite (per-object tuples,
  B-tree-indexed catalog). That is a *separate*, larger item and only needed if
  the catalog must scale to huge *schemas* (thousands of tables/columns), which is
  not what this limit is about.
- The M10 vacuum already reclaims/compacts heap pages; F2's durable FSM should
  reuse M10's free-space accounting so the two don't drift.
