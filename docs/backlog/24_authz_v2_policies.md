# AuthZ v2 — SQL-native roles/grants, per-operation RLS policies, WITH CHECK

**Type:** Milestone
**Status:** ✅ SHIPPED (all Z-series + R-hardening) — see PROGRESS.md "Item 24"
- Z1+Z3(JWT)+Z5: `feat/item-24-authz-z1z3z5` (2026-07-19, PR #167)
- Z4 role_members/users catalog: PR #166 (2026-07-19)
- Z6 current_user + POST /auth/preview: `feat/item-24-z6-current-user-preview` (2026-07-19)
- R-a UPDATE WITH CHECK enforcement + R-b bootstrap enforced column: `feat/item-24-rls-hardening-login` (2026-07-20)

<!-- Shipped: commit 6ad38db on branch feat/item-24-authz-z1z3z5 -->
<!-- Z1: CREATE/DROP ROLE/POLICY, GRANT/REVOKE — catalog-persisted, insert_policy + rls_policy routing -->
<!-- Z3(JWT): POST /tables/{name}/bulk enforces INSERT grant, returns 403 -->
<!-- Z5: unidb_catalog.roles / .grants / .policies virtual catalog relations -->
<!-- Shipped: branch feat/item-24-z6-current-user-preview (2026-07-19) -->
<!-- Z6: Expr::CurrentUser + substitute_current_user_in_plan; apply_rls_skip_current_user for embedded path; -->
<!--     POST /auth/preview (superuser-only impersonation + RLS preview); 4 unit tests + 2 server tests -->

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

## Remaining work (2026-07-20 fresh-mind review — live-probe confirmed)

Z1/Z2/Z3(JWT)/Z5/Z6 shipped. Three items remain, two of them security-critical:

### R-a — Z3 write-side `WITH CHECK` on UPDATE  🔴 SHIP-BLOCKER

**Confirmed by live probe** (archived: scratchpad `withcheck_escape_probe.rs`), on `main`
at commit 196e8aa:

```
policy: user_id = current_user  (FOR SELECT and FOR UPDATE)
alice runs: UPDATE todos SET user_id = 'bob' WHERE id = 1
RESULT: accepted — alice updated 1 row; alice now sees 0 rows;
        bob now sees alice's row. Ownership transferred OUTSIDE the policy.
```

Root cause: `exec_update` applies `update_policy` as a **USING** filter (which rows may be
touched) but never validates the **NEW** row. INSERT already does the right thing
(`insert_policy` → `check_passes(policy, coerced)` at `executor.rs:1674`). Postgres defaults
UPDATE `WITH CHECK` to the `USING` expression when no explicit `WITH CHECK` is given.

Fix (contained): after the SET is applied and the row coerced in `exec_update`, evaluate the
UPDATE policy's WITH-CHECK expression (explicit `WITH CHECK` if the policy carries one, else
the `USING` predicate) against the new row via the existing `check_passes`; reject with the
same error class as the INSERT path. Must also fire on the RC re-evaluation path (landmine 2).
Parser: accept optional `WITH CHECK (expr)` on `CREATE POLICY` and store it (new catalog
field, `#[serde(default)]` — verify no FORMAT_VERSION bump needed).

### R-b — bootstrap-mode silent non-enforcement  🟡

Discovered by the same probe: with `CREATE ROLE` (not `CREATE USER`) the SELECT policy did
**not** filter — enforcement only activates once ≥1 `CREATE USER` exists (deliberate bootstrap
escape, landmine 3). An operator who creates roles + policies but no USER believes RLS is on
while it is fully off, with no signal. Fix: surface the state — a startup/`whoami` warning when
policies exist but bootstrap mode is active, and an `enforced: false` column on
`unidb_catalog.policies` in that state. Document explicitly.

### R-c — Z4 role inheritance + column-level grants (Should; scope-check first).

## Acceptance

- [ ] **R-a**: the archived escape probe, inverted (expect rejection), passes; WITH CHECK proven
      on all four DML ops incl. UPDATE ownership-transfer and RC re-evaluation. 🔴
- [ ] **R-b**: policy-exists-but-bootstrap-active is observable (warning + catalog flag); a test
      asserts a policy is reported non-enforcing until first `CREATE USER`. 🟡
- [ ] Concurrency matrix extended with an RLS cell (readers under policies
      during churn) — green at CONC_REPEATS=10.
- [ ] Studio Auth tab works via catalog + SQL only (no bespoke endpoints).
- [ ] Audit log records policy/grant changes with acting principal.
- [ ] **Performance gates** (protect the CRUD numbers): Table 3 with no policies is
      unchanged within noise; an RLS-on vs manual-`WHERE` comparison on an indexed policy
      column is within ≤10%.
