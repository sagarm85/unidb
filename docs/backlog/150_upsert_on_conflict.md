# 150 — Upsert: `INSERT … ON CONFLICT` (+ PostgREST upsert wiring)

**Type:** Improvement
**Status:** IN PROGRESS (user go-ahead 2026-08-03; branch `feat/150-upsert-on-conflict`)

> Compute-cluster next phase, and the FIRST item since M1 to deliberately
> extend the ACID write path. Removes item-139's documented exclusion:
> PostgREST `resolution=merge-duplicates` needs `ON CONFLICT`, which the
> engine didn't have. **Design rule: the update arm is executed by the
> EXISTING update machinery** (MVCC insert-new-version/HOT, index
> maintenance, FK checks, locks) — this item adds routing, not a new write
> path.

## Grammar (locked v1)

```
INSERT INTO t (cols) VALUES (...)[, (...)]
  ON CONFLICT [(col)] DO NOTHING
| ON CONFLICT (col)   DO UPDATE SET col = expr [, ...] [WHERE cond]
```

- Conflict target: **exactly one column**, which must be the PK or carry a
  UNIQUE index. Optional for `DO NOTHING` (then any unique violation is
  ignored); **required** for `DO UPDATE`. Composite targets, `ON CONSTRAINT
  <name>`, and `MERGE` are v1 non-goals.
- `EXCLUDED.<col>` usable in the `SET` exprs and the `DO UPDATE WHERE`,
  referring to the row proposed for insertion.
- Parse via the pinned sqlparser's `OnInsert`/`OnConflict` AST if supported
  (implementer verifies empirically, as item 148 did); pre-parse fallback
  only if the AST route is unavailable.

## Semantics (locked v1)

Per proposed row, in `exec_insert`:
1. Probe the conflict target's unique index for a **live, visible**
   conflicting row — reuse the existing `enforce_unique` probe + phantom/
   unique locking, so concurrent same-key upserts serialize exactly like
   concurrent conflicting INSERTs do today.
2. No conflict → normal insert (all existing behavior: WITH CHECK, FK,
   indexes, HNSW, events).
3. Conflict + `DO NOTHING` → skip the row silently; the statement reports
   rows actually inserted (Postgres semantics). Not an error.
4. Conflict + `DO UPDATE` → evaluate `SET` (with `EXCLUDED.*` bound to the
   proposed row) and optional `WHERE` against the existing row; if `WHERE`
   fails → row is skipped (Postgres semantics). Otherwise route into the
   existing single-row UPDATE machinery.
5. **NULL never conflicts** (SQL unique semantics — must match today's
   unique-index NULL behavior; add a test proving parity).

**RLS/authz (fail-closed):** the insert arm enforces INSERT `WITH CHECK`
as today. The update arm enforces the caller's **UPDATE** policies: `USING`
must match the target row (mismatch = error, not silent skip — document the
divergence-from-skip choice: silently skipping on RLS would leak existence)
and `WITH CHECK` applies to the post-image. Column grants: caller needs
INSERT grants on inserted cols and UPDATE grants on SET cols. `RETURNING`
works on both arms through the existing `check_returning` path.

**Transactions/crash:** both arms are ordinary WAL-logged heap ops inside
the statement's mini-txn/user-txn — atomicity needs no new machinery. Add
one targeted crash test: kill between the upsert-update's WAL append and
page flush; reopen; assert the row is entirely old or entirely new version
(and the unique index resolves to exactly one live row).

## REST wiring (removes 139's exclusion)

`POST /rest/v1/<table>`: honor `on_conflict=<col>` query param with
`Prefer: resolution=merge-duplicates` (→ `DO UPDATE SET` every writable
column = `EXCLUDED.<col>`) and `Prefer: resolution=ignore-duplicates`
(→ `DO NOTHING`). Composes with existing `return=` handling and the
`in.(...)` merge logic. No `Prefer` → existing behavior byte-identical.
GraphQL upsert = non-goal (follow-up).

## Files (expected — implementer verifies against real code)

`src/sql/parser.rs` (+ AST mapping), `src/sql/logical.rs` (OnConflict on
the Insert plan node), `src/sql/executor.rs` (`exec_insert` routing +
`EXCLUDED` binding), `src/error.rs` (invalid conflict target), REST:
`src/server/rest_resource.rs` (139's `parse_prefer` home). Docs:
`docs/sql/sql_reference.md` INSERT section, `docs/REST_API.md` (upsert
under the `Prefer` section, remove the "not supported" note), `README.md`
SQL bullet, update `139_rest_count_prefer.md`'s exclusion note to point
here. Tests: `tests/item150_upsert.rs` (engine, not feature-gated) + REST
cases in a `#![cfg(feature = "server")]` file or the item150 file split.

## Required tests

1. DO NOTHING: duplicate skipped, non-duplicate inserted, correct counts;
   with and without explicit target.
2. DO UPDATE: EXCLUDED values land; WHERE-guarded arm skips when false;
   works on PK and on a secondary UNIQUE column.
3. RETURNING on both arms.
4. NULL-never-conflicts parity with plain INSERT.
5. RLS: insert arm WITH CHECK parity; update arm USING mismatch errors;
   post-image WITH CHECK enforced; column-grant denial (SET col without
   UPDATE grant).
6. FK interplay: upserting a child row validates FK on both arms.
7. Concurrency: two threads upserting the same new key — exactly one row
   exists after both commit (one inserted, one updated or write-conflict
   aborted per current lock semantics — assert no duplicate and no hang).
8. Vector/index maintenance: upsert-update on an HNSW/B-tree-indexed column
   maintains the index (existing update machinery — prove it routed there).
9. Crash test (see above) + harness stays green.
10. REST: `on_conflict` + both `Prefer: resolution=` modes, incl. RLS
    parity and no-Prefer unchanged-behavior regression.

## Verification gates

`cargo build` · `--features server` · clippy `--all-features --all-targets
-D warnings` · fmt · plain `cargo test --no-run` · item150 suite ·
regressions: `constraints`, unique/FK suites, `server_rest`, `item139`
prefer tests · **crash harness green including the new point** (previous 54
must stay green; the new test extends the count).

## Non-goals (document each)

Composite conflict targets · `ON CONSTRAINT` form · MERGE · GraphQL upsert ·
`DO UPDATE` referencing other tables · conflict on expressions/partial
indexes.
