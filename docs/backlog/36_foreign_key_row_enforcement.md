# Item 36 — Foreign keys enforce table existence only, not referential integrity

**Type:** Improvement
**Status:** ✅ SHIPPED 2026-07-14 — See PROGRESS.md for metrics and PR link.
**Priority:** High — correctness/integrity, but **sequenced after item 35** (it
reuses the parent-table PK index that item 35 builds; doing it before 35 would
reintroduce the same O(n) per-row scan item 35 exists to remove).

---

## Problem

A `FOREIGN KEY` today enforces only that the **referenced table exists in the
schema** — never that the referenced **row** exists. This means:

- `INSERT INTO order_items(order_id) VALUES (999999)` **succeeds** even when no
  `orders` row has `id = 999999`. The FK is recorded, shown in
  `information_schema` and drawn in the ERD, but the database does not protect
  it — a dangling reference is accepted silently.
- Deleting a parent row that still has children is **not** blocked and does
  **not** cascade — there is no `RESTRICT`, `CASCADE`, or `SET NULL` behavior.

This is a **documented, deliberate M11 scope decision**, not an accidental bug
(`src/catalog.rs:132-140`):

> "Enforcement in M11 is deliberately limited to referenced-table existence …
> full referential integrity (referenced *row* existence, `ON DELETE`/`ON
> UPDATE` actions) is out of scope, since there is no `DROP TABLE` yet and
> row-level FK checks are a materially larger lift…"

`ForeignKeyRef.column` is already recorded on the catalog "for a future
row-existence check, but only the `table` is enforced today"
(`src/catalog.rs:145-147`). This item is that future check.

**Honesty note (corrects an earlier in-session claim):** FK constraints were
described as "parsed, enforced (`ForeignKeyViolation`), and introspectable." The
accurate statement is: FK is parsed, introspectable, and enforced **only for
referenced-table existence** — value-level referential integrity was
deliberately deferred, and this is where it lands.

## Current state / root cause

- `enforce_referenced_tables_exist()` (`src/sql/executor.rs:2090`) is the only
  FK enforcement. It calls `catalog.lookup(ref_table)` and raises the **sole**
  `DbError::ForeignKeyViolation` in the codebase (`:2093`) when the referenced
  *table* is missing. It never inspects the FK column's value.
- No `CASCADE` / `RESTRICT` / `ON DELETE` / `ON UPDATE` handling exists anywhere
  (grep: the only `referencing` uses are in `sql/information_schema.rs`, for the
  read-side projection — display, not enforcement).
- There is no `DROP TABLE` yet; parent removal happens via row `DELETE`, so the
  parent-side action to design first is **row delete/update**, not table drop.

## Why this depends on item 35 (and is cheap after it)

A row-existence FK check is a **point lookup into the parent table's `PRIMARY
KEY`** ("does a visible parent row with this key exist?"). That index is exactly
what **item 35** builds (`DiskBTree`-backed PK/UNIQUE). So:

- **After item 35:** each FK check is O(log n) — reuse the parent's PK index.
- **Before item 35:** each FK check would be another full `heap.scan()` of the
  parent per child row — i.e. the *same* O(n²) mistake item 35 removes, freshly
  reintroduced on the FK path.

Therefore: **land item 35 first, then reuse its index machinery here.** Do not
implement an interim scan-based FK check.

## Proposed scope (phased; re-derive against MVCC per CLAUDE.md §0.6.2)

### Phase 1 — child-side: referenced row must exist
On `INSERT`/`UPDATE` of a row carrying an FK, verify the referenced parent key
exists and is **visible** to the writer, via a point lookup into the parent's PK
index (item 35). Raise `ForeignKeyViolation` (extend the error with the
offending value/column) when it doesn't.

### Phase 2 — parent-side: `DELETE`/`UPDATE` of a referenced row
Default **`RESTRICT`**: reject deleting/updating a parent row while a visible
child still references it (a point/range lookup on the referencing side — needs
an index on the child FK column, or a documented scan fallback with its own
scale test). `ON DELETE CASCADE` / `SET NULL` and `ON UPDATE` actions are an
**optional** follow-up, not required by this item's first cut — parse-and-store
first, enforce `RESTRICT` correctly, then extend.

## Correctness invariants the fix MUST preserve (each needs a test)

The FK check has the same MVCC hazards as item 35's uniqueness check — it reads
another row's visibility under concurrency:

1. **MVCC visibility.** The parent row must be *visible-or-committed* under the
   child writer's snapshot. A parent inserted by an uncommitted *other* txn must
   not satisfy the FK (or must block), and a parent deleted-but-still-visible to
   an older snapshot must be handled per isolation level.
2. **Own-xid / same-transaction.** A parent inserted earlier in the *same*
   transaction (not yet committed) **must** satisfy a child FK in that
   transaction — the common "insert parent then child" pattern and the bulk-load
   case.
3. **Concurrent parent-delete vs child-insert race** (single-primary, concurrent
   writers, item 11). The classic FK hazard: child-insert validates the parent,
   parent-delete commits, child commits → dangling. Requires a share/row lock on
   the parent (à la Postgres `SELECT … FOR KEY SHARE`) or an equivalent
   conflict, proven under `benches/conc_matrix.rs`.
4. **NULL FK columns are not checked.** A NULL foreign-key value is permitted
   (SQL semantics) — skip the check when the FK column is NULL.
5. **Performance.** The check is O(log n) point lookups, **not** an O(n) scan —
   a scale regression test (child inserts into a large parent) must stay flat,
   the same bar item 35 sets. If Phase 2's referencing-side check needs a scan,
   it must be indexed or the scan cost documented and bounded.

## Acceptance criteria

- [ ] Child `INSERT`/`UPDATE` referencing a **non-existent** parent key is
      rejected with `ForeignKeyViolation` (naming the column/value); referencing
      an **existing, visible** parent succeeds.
- [ ] `DELETE`/`UPDATE` of a parent row still referenced by a visible child is
      rejected (`RESTRICT`); with no referencing child, it succeeds.
- [ ] **Same-transaction** insert-parent-then-insert-child succeeds (invariant
      2); NULL FK value is accepted unchecked (invariant 4).
- [ ] **Concurrency:** a `conc_matrix.rs` cell proving no dangling reference
      under concurrent parent-delete / child-insert (invariant 3).
- [ ] **Performance:** child inserts into a large (100k+) parent stay flat
      (O(log n) point lookup), not O(n) — a permanent regression test, and no
      regression to the multi-model report Tables 1 & 4 (per item 35's format)
      if the FK path touches write throughput.
- [ ] Existing FK table-existence rejection still passes unchanged.
- [ ] **Docs corrected, not silently rewritten** (§9): update
      `src/catalog.rs:132-147` `ForeignKeyRef` scope comment and
      `enforce_referenced_tables_exist`'s doc from "table existence only" to the
      enforced behavior; flip `ForeignKeyRef.column` from "informational" to
      enforced; update `docs/engine_access_guide.md` / `docs/REST_API.md` for the
      new error condition; record the M11-scope graduation in `PROGRESS.md`.

## Depends on / builds on

- **Item 35** (index-backed PK/UNIQUE) — hard dependency: this reuses the parent
  PK index item 35 builds; do not implement before it.
- Item 11 (concurrent writers) — the parent-delete/child-insert race lives here.
- The `DiskBTree` machinery — the mechanism to reuse for both the parent-key
  lookup and (Phase 2) the referencing-child lookup.
