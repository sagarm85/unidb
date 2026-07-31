# 112 — Column-level grants (the deferred half of item 24 Z4)

**Type:** Improvement
**Status:** ✅ SHIPPED (2026-07-31) — implemented as item-122 Workstream B5.
Grant vocabulary + persistence, DDL, read/write/RETURNING enforcement
(incl. `SELECT *`/QuerySpec join shapes), the policy-column exemption,
`information_schema.columns` filtering, and the fast-path audit are all
done — see the session log in this repo's history for the commit(s) and the
gate results (`cargo test --features server`, crash 54/54). Filed 2026-07-22
so the scope was decided deliberately, not rediscovered; un-parked and
implemented 2026-07-31.

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

## Acceptance

- [x] Step-0: enumerate every plan shape's column-reference extraction and
      decide the policy-column exemption rule BEFORE coding. **Decision:**
      `check_plan_privileges`/`check_column_privileges` run on the plan as
      parsed from the caller's own SQL — `parse_sql_cached`'s result —
      strictly *before* `apply_rls_with_auth` ever injects a policy
      predicate (that happens later, inside `execute_sql_inner_as_principal`,
      over a value this check never sees). So the policy-column exemption
      falls out of call ordering, with no special-casing needed inside the
      extraction code itself. Column-reference extraction: `Expr`
      (single-table `Select`/`Update`/`Delete`/`Insert…RETURNING`) via
      `sql::logical::collect_expr_columns`; `QExpr`/`QuerySpec` (joins,
      aggregates, subqueries, CTEs) via `sql::query::collect_query_column_reads`,
      which resolves qualifiers against the FROM tree and fails closed
      (charges every candidate real table) on an unqualified/ambiguous
      reference across ≥2 joined tables.
- [x] Grant/revoke DDL + persistence + catalog view field.
- [x] Read/write/RETURNING enforcement incl. `SELECT *` and QuerySpec
      shapes; error (not mask) on ungranted columns.
- [x] `information_schema.columns` filtered per column grants (item 111
      extension).
- [x] Fast-path audit recorded (see `check_column_privileges`'s doc comment
      in `lib.rs`): the check runs on the `LogicalPlan` before any physical
      planning begins, so parallel scan / index-only (covering) / O(1)
      `COUNT(*)` — all chosen later, from an already-checked plan — cannot
      bypass it. `cargo test --test crash` stays 54/54.
