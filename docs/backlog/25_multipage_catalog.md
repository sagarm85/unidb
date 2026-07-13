# Multi-page catalog — lift the single ~8 KiB catalog-blob ceiling

**Type:** Improvement
**Status:** SHIPPED — see `PROGRESS.md` entry "Multi-page catalog (item 25)"

## Design decisions (resolved before coding — 2026-07-13)

### Landmine 1 — Format marker / version bump?
**Decision: NO `FORMAT_VERSION` bump.** The multi-page layout is fully
self-describing from the first catalog page's slot-0 payload. Each page in a
chain begins with a 4-byte little-endian magic (`CATALOG_CHAIN_MAGIC =
0xC0DA_7A10u32`). Detection in `Catalog::load`: if the first 4 bytes of slot 0
equal the magic, follow the chain; otherwise fall back to the legacy raw-JSON
path. Old JSON always starts with `{` (0x7B in ASCII); the magic's first LE byte
is 0x10, so the two formats are unambiguously distinguishable without any version
field. No §3/D9 sign-off ritual required.

### Landmine 2 — Atomicity under crash
**Decision: write-new-chain-then-flip pattern.** `persist` allocates all chain
pages, WAL-logs and buffer-pool-writes them all in **one mini-txn**, and only
**then** rewrites `catalog_root` in the control file. The control file is the
single atomic commit point (a 44-byte write that is either present or not).
Crash before the control file flip → old `catalog_root` intact, new pages are
unreachable orphans (same trade-off as old catalog pages becoming garbage on
each rewrite today). Crash after the flip → new chain is WAL-recovered and fully
readable. The WAL commit (`commit_mini_txn` fsync) is also guaranteed to precede
the control file write, so the chain is durable before it becomes reachable.
Crash point P33 exercises this.

### Landmine 3 — D5 compliance for multi-page write
**Decision: same discipline as today, extended.** Each chain page is WAL-logged
via `log_insert` (under the same mini-txn) before `pool.write_page`, preserving
the WAL-before-page invariant (D5) for every new page. The single mini-txn that
covers all N pages means the commit record is fsynced once, after all
`log_insert` calls, before any of the pages could be evicted ahead of the WAL.

> The whole catalog — **every `TableDef` plus every table's `TableStats`** — is
> persisted as **one JSON blob in slot 0 of a single meta page** (`catalog.rs::
> persist`). Once that blob exceeds the ~8 KiB page, the next catalog write dies
> with `HeapFull { size }`. This caps the total schema (tables × columns) **and**
> the accumulated statistics the database can hold, and it fails in a
> particularly nasty way: a *runtime* catalog mutation (a lazy `CREATE TABLE`, an
> `ANALYZE`, a `SERIAL` allocation) can overflow long after the schema was
> created, once `stats`/page-lists have grown. Item 10
> (`durable_fsm_catalog_pagelist.md`) moved **page directories** out of this blob;
> this item finishes the job for **table defs + stats**.

## Evidence (measured while building item 23)

The object-storage service (`unidb-storage`, [item 23](23_storage_service.md))
hit this wall directly. Concrete numbers on a default 8 KiB page, fresh engine:

- `buckets`(3 cols) + `objects`(11 cols incl. `storage_key`) + the 8-column
  `unidb_dispatch::dlq` table → **`HeapFull { size: 9096 }`** at `CREATE TABLE`.
- Dropping `storage_key` (→ 10-col `objects`) + the 8-col DLQ → still
  **`HeapFull { size: 8883 }`**.
- 10-col `objects` + a **compact 4-col** DLQ → fits, but with **~1 column of
  headroom** (adding one `INT` column overflows).
- After **3 000 row inserts** into a fitting schema, the *next* catalog write
  (an `enable_events` / `ANALYZE`) serialized **9 651 bytes → `HeapFull`** — i.e.
  the in-memory catalog grows with row volume and any later catalog mutation
  re-serializes the grown blob and overflows.

Item 23 shipped by **working around** this in the service layer (compact schema,
`storage_key` derived not stored, **all DDL done up front** in
`StorageService::new` so no runtime catalog write happens). That is a real
constraint on *every* multi-table app layer built on unidb, not just storage.

## Root cause

`Catalog::persist` (`catalog.rs`) does:

```
let encoded = serde_json::to_vec(PersistedCatalogRef { tables, stats });
let mut page = SlottedPage::new(page_id, PAGE_TYPE_META, page_size);
let slot = page.insert(&encoded)?;   // <- HeapFull here when encoded > page free space
debug_assert_eq!(slot, 0, "catalog page must hold exactly one blob at slot 0");
```

Two things share this one page and both grow unboundedly:

1. **`tables`** — every `TableDef` (name, column defs, constraints, FK metadata).
   Grows with schema width.
2. **`stats`** — every table's `TableStats` (row count + per-column
   distinct/null/min/max + an 8-bucket histogram). Grows as tables are
   `ANALYZE`d; the `statistics.rs` module comment already flags that histograms
   "must not balloon" the single blob *because of this ceiling*.

Catalog writes happen on every DDL, `enable_events`, `set_table_stats`
(`ANALYZE`), and **`alloc_serial`** (`catalog.rs` — *every* `SERIAL` insert
rewrites the catalog), so the overflow can surface at runtime, not just at
schema-creation time.

## Why it matters (impact)

- **Hard cap on schema size** — roughly a dozen-and-a-half columns across a
  handful of app tables (on top of the system tables `__events__`,
  `__consumers__`, `__lobs__`) is enough to overflow. Any real application
  outgrows this.
- **`ANALYZE` is effectively unusable on a near-full catalog** — it fails with
  `HeapFull` instead of collecting stats, silently degrading the planner.
- **Latent runtime failure** — a schema that fits at creation can overflow later
  via `ANALYZE`/`SERIAL`/a lazy `CREATE TABLE`, which is a surprising,
  hard-to-diagnose production failure mode.
- **Forces unnatural app design** — item 23 had to drop a column and front-load
  all DDL purely to fit; future service layers ([item 24](24_authz_v2_policies.md)
  authz policies, any studio-managed schema) will face the same tax.

## Options (rank by ROI before implementing — CLAUDE.md §0.6)

1. **Split `stats` out of the catalog blob (smallest, highest ROI first).** Keep
   `tables` in the blob (defs are comparatively small and change rarely) but move
   `TableStats` to their own durable storage — e.g. a per-table stats page/row
   keyed by table id, loaded lazily. This removes the *growing* half from the hot
   blob and makes `ANALYZE` scale, with the least churn to recovery. Likely
   clears the immediate item-23-class pain on its own.
2. **Multi-page catalog blob.** Chain the encoded catalog across N meta pages
   (length-prefixed, page-linked) so `tables + stats` can exceed one page.
   Touches `persist` + `load` + recovery (`control.catalog_root` becomes a chain
   head). Straightforward but spreads a format change into recovery.
3. **Self-hosting catalog table.** Store catalog entries as ordinary rows in an
   FSM-backed system heap (the way user tables already scale past the old
   page-list ceiling, item 10), instead of one JSON blob. Cleanest long-term
   (uniform with the rest of the engine, no bespoke chaining) but the largest
   change — bootstrap/recovery must read the catalog table before the catalog is
   available.

**Recommendation to evaluate first:** Option 1 (split stats out) as a targeted
fix, then decide whether Option 2/3 is warranted for `tables` growth. Do the ROI
re-derivation against the actual failure (the blob overflowed at ~8.1–9.7 KiB
with stats being the runtime-growing contributor) rather than assuming.

## Landmines / constraints

- **On-disk format + recovery.** Options 2/3 change how the catalog is persisted
  and read at open; recovery (`recovery.rs`, `control.rs`
  `catalog_root`) must handle both the old single-page and new layouts, or bump
  `FORMAT_VERSION` (a **locked-decision-adjacent** change — D9; get sign-off and
  record it). Option 1 can likely avoid a format bump if stats storage is
  additive and absence-tolerant.
- **Crash safety.** The catalog write is WAL-logged as a mini-txn today
  (`persist`); any multi-page/multi-row scheme must stay atomic under the crash
  harness (D7) — a torn multi-page catalog on recovery is unacceptable.
- **Bootstrap ordering (Option 3).** A catalog-as-table must be openable before
  the catalog exists — a fixed root page / self-describing bootstrap entry.
- **Don't regress the item-10 win.** Page directories must stay in the durable
  FSM/`DiskBTree`, not migrate back into whatever new catalog store this builds.

## Acceptance

- [ ] A schema well past today's ceiling (e.g. **item 23's original layout**:
      `objects` *with* `storage_key` + the full 8-column dispatch DLQ, plus room
      to spare) is created and used with **no `HeapFull`**.
- [ ] `ANALYZE` succeeds on a many-table database that would overflow the single
      blob today; repeated `ANALYZE`/`SERIAL`/DDL after heavy row volume never
      overflows the catalog.
- [ ] Crash harness stays green across the new catalog persist/recovery path
      (D7); a crash mid-catalog-write recovers to a consistent catalog.
- [ ] Reopen compatibility: an existing single-page-catalog database opens and
      upgrades cleanly (or a `FORMAT_VERSION` bump + documented migration, signed
      off per D9).
- [ ] Once shipped, item 23's service-layer workaround can be relaxed (restore
      `storage_key` as a real column / allow runtime DDL) — verify the storage
      tests still pass without the compaction.

## Depends on / relates to

- Extends **item 10** (`durable_fsm_catalog_pagelist.md`, SHIPPED) — same blob,
  the remaining growing contents (defs + stats).
- Surfaced by **item 23** (`23_storage_service.md`, SHIPPED) — see its
  `docs/design/storage_service.md` §4 dated correction for the workaround.
- Unblocks cleaner schemas for **item 24** (authz v2 policies) and any
  studio-managed / multi-table app layer.
