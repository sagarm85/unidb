# AuthZ v2 — SQL-native roles/grants, per-operation RLS policies, WITH CHECK

**Type:** Milestone
**Status:** NOT STARTED

> Deliberately LAST of the Supabase-track items (deep semantics; every earlier
> item — catalog, logs, metrics — is the debugging surface for hardening this).
> Builds on what P6.e/P6.f + item 12 already shipped: `RoleStore` + GRANT
> enforcement (`execute_sql_as`), per-user JWT `sub` (verify-only — issuance
> stays OUT, per Milestone 18 non-goals), TLS, audit log, and single-predicate
> RLS via `set_rls_policy_sql` (planner AND-rewrite).

## Gaps this closes (Supabase-grade authz)

- **Z1 — SQL-native DDL:** `CREATE/DROP ROLE`, `GRANT/REVOKE`, `CREATE/DROP
  POLICY` as statements (today: JSON/REST only). Persisted in the catalog, not
  `roles.json` sidecars — migration path documented.
- **Z2 — Per-operation policies, multiple per table:** policy carries an
  operation scope (SELECT/INSERT/UPDATE/DELETE/ALL) and tables hold many
  policies (OR-combined per Postgres semantics for permissive policies).
- **Z3 — Write-side `WITH CHECK`:** the classic RLS hole — today's rewrite is
  read-shaped; INSERT/UPDATE must validate the NEW row against the policy or
  reject. This is the correctness core of the milestone.
- **Z4 — Role inheritance + column-level grants** (Should; scope-check before
  committing — column grants may defer).
- **Z5 — Catalog exposure:** `unidb_catalog.{roles,grants,policies}` relations
  (item-18 shape) so the studio Auth tab is just catalog queries + Z1 DDL.
- **Z6 — Studio Auth tab:** role CRUD, policy editor (SQL predicate), and
  **"preview as user"** — run a query via `execute_sql_as` to see exactly what
  a role sees (the killer debugging feature).

## Landmines (decide first, per CONVENTIONS de-risk)

1. Policy evaluation vs the parallel-scan + count-visible fast paths: RLS
   rewrite must force the predicate onto EVERY read route (COUNT(*) header-only
   shortcut must be disabled for RLS tables or made policy-aware — item-16
   lesson: fast paths are where visibility bugs hide).
2. `WITH CHECK` under RC re-evaluation: re-evaluated rows must re-check policy.
3. Superuser/bootstrap semantics and `BYPASSRLS` equivalent — explicit, audited.
4. `roles.json` → catalog migration without breaking existing deployments
   (`#[serde(default)]` precedent; no FORMAT_VERSION bump expected — verify).

## Acceptance

- [ ] Write-skew-style policy escape attempts covered by tests: a role can
      neither read nor WRITE outside its predicate (Z3 proof, all four DML ops).
- [ ] Concurrency matrix extended with an RLS cell (readers under policies
      during churn) — green at CONC_REPEATS=10.
- [ ] Studio Auth tab works via catalog + SQL only (no bespoke endpoints).
- [ ] Audit log records policy/grant changes with acting principal.
