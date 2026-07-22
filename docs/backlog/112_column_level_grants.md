# 112 — Column-level grants (the deferred half of item 24 Z4)

**Type:** Improvement
**Status:** ⏳ NOT STARTED — filed 2026-07-22, parked until a concrete user
need appears (RLS policies already cover row/ownership shaping; nothing in
the Studio integration has hit a column-granularity wall). Filed now so the
scope is decided deliberately, not rediscovered.

## Status clarification (what Z4 actually was)

Item 24's Z4 was specced "role inheritance + column-level grants (Should;
column grants may defer)". Audit 2026-07-22:

- **Role inheritance: SHIPPED and working** — `RoleStore::has_privilege`
  resolves transitively over memberships (worklist; nested role-in-role
  chains included); `unidb_catalog.role_members`/`.users` landed in PR #166.
- **Column-level grants: never implemented** — grants are whole-table only.

This file owns the unshipped half; item 24 is otherwise complete.

## Feature

```sql
GRANT SELECT (email, name) ON users TO support;      -- not password_hash
GRANT UPDATE (status) ON tickets TO agent;           -- not owner_id
REVOKE SELECT (email) ON users FROM support;
```

Postgres semantics: a column grant is a *narrowing* of the table privilege;
holding table-level SELECT implies all columns; column-level SELECT admits
only the listed columns (a `SELECT *` or an unlisted column → permission
denied, not silent masking — Postgres errors, it does not NULL-fill).

## Touch points (why this is wide — the reason it was deferred)

1. **Grant vocabulary + persistence:** `Privilege` gains column scope;
   `RoleStore` grant storage `(grantee, table) → {priv}` becomes
   `(grantee, table) → {priv → all | cols}`; catalog serialization bump for
   the authz store (check its on-disk format versioning).
2. **DDL:** `GRANT/REVOKE <priv> (col, …) ON t TO r` parse + apply;
   `unidb_catalog.grants` gains a `columns` field (Z5 view change).
3. **Read path:** `check_plan_privileges` must validate the *projection and
   predicate columns* of Select/QuerySpec plans against column grants —
   including `SELECT *` expansion, expressions, GROUP BY/ORDER BY refs, and
   columns referenced only inside RLS-injected predicates (policy columns
   must be readable-by-policy even when not caller-granted — Postgres treats
   policy evaluation as exempt; decide and document).
4. **Write path:** UPDATE assignment targets and INSERT column lists checked
   per-column; RETURNING columns count as reads.
5. **Item 111 interaction:** `information_schema.columns` must list only the
   granted columns for a column-scoped grantee (the ANY-privilege table
   filter stays for `tables`).
6. **Fast paths:** parallel scan, index-only (102-A/B covering), COUNT(*)
   O(1) — each must be audited for a route that materializes an ungranted
   column (same class of hazard as item 24's landmine 1).

## Acceptance (draft — refine at Step 0)

- [ ] Step-0: enumerate every plan shape's column-reference extraction and
      decide the policy-column exemption rule BEFORE coding.
- [ ] Grant/revoke DDL + persistence + catalog view field.
- [ ] Read/write/RETURNING enforcement incl. `SELECT *` and QuerySpec
      shapes; error (not mask) on ungranted columns.
- [ ] `information_schema.columns` filtered per column grants (item 111
      extension).
- [ ] Fast-path audit recorded; conc matrix + full suite + crash green.
