# RLS ↔ token binding — auth.uid() / auth.jwt() / roles (Workstream B)

**Type:** Milestone
**Status:** SHIPPED (2026-07-31) — B1 (`auth.uid()`) + B2 (`auth.jwt() ->> 'claim'`) SHIPPED on
branch `claude/permissions-security-supabase-comparison-ixho2w` (commits `3fb0e04` +
QExpr-path fix `2a5fe85`; item122 tests 7/7 incl. two-tenant isolation, fail-closed,
and the LIMIT/QExpr path; crash 54/54). Both fail closed (Null, never Bool(true) —
item-110 lesson). **B3 (built-in `anon`/`authenticated`/`service_role` roles) + B4
(role-scoped policies `… TO <role>`) SHIPPED** same branch (`838d91d`): effective roles
resolved engine-side; `service_role` bypasses RLS on the audited path (item-103); no-`TO`
policies still apply to all callers (back-compat), `TO`-scoped gated on role intersection,
exclusively-scoped-and-no-match fails closed; reserved role names rejected. item122 7/7 +
item122_b3_b4 6/6, crash 54/54. **B5 (column-level grants) SHIPPED** (2026-07-31, same
branch): see item 112 for the full detail — grant model + DDL + `check_plan_privileges`
column enforcement (incl. QuerySpec join/aggregate shapes, fail-closed on ambiguity) +
`information_schema.columns` filtering + fast-path audit; item112_column_grants 11/11,
crash 54/54. Workstream B is now fully shipped.

> Makes the token's *identity, claims, and role* usable inside RLS policies — the
> Supabase model — instead of only the username. Part of the
> [Supabase parity roadmap](120_supabase_parity_roadmap.md) (Wave 1, P0).
> Independent of Workstream A (121); coordinate only on where the request's claims
> attach to the exec context in `handlers.rs`.

## Problem (verified in code)

The token→RLS connection already works, but only through `current_user` (= JWT
`sub`), via `apply_rls(plan, catalog, Some(user))` +
`substitute_current_user_in_plan` (`src/sql/logical.rs`). What is missing is the
*richness* Supabase policies depend on:

- No `auth.uid()` — a stable subject id distinct from a display username.
- No `auth.jwt() ->> 'claim'` — arbitrary token claims (e.g. `tenant_id`) are
  unreachable from a predicate, so multi-tenant policies cannot be written.
- No built-in `anon` / `authenticated` / `service_role` roles — Supabase's whole
  security model keys on these.
- Policies are not role-scoped: a policy applies to every caller, so there is no
  `CREATE POLICY ... TO authenticated` equivalent.

## Scope

### B1 — `auth.uid()` (MUST)
- New `Expr::AuthUid` leaf (mirrors `Expr::CurrentUser`), substituted at
  policy-injection time from the token's subject id. Fail **closed** (Null + warn)
  when absent, exactly like the item-110 `current_user` hardening — never fall back
  to a `Bool(true)` that weakens the policy.
- Requires the request's authenticated subject to reach `apply_rls`; extend the
  exec context (`ExecCtx`) to carry an auth principal, not just a username string.

### B2 — `auth.jwt() ->> 'claim'` (MUST)
- Thread the verified token claims (a small `HashMap<String, Value>`) from
  `require_jwt` (`src/server/auth.rs`) into `ExecCtx`.
- New `Expr::AuthClaim(String)` leaf; substituted with the claim's literal value at
  injection time (typed as text; cast in-predicate as needed). Absent claim → Null
  (fail closed).
- Only claims from a *verified* token are ever exposed — no unauthenticated path
  can populate them.

### B3 — Built-in roles (MUST)
- Seed `anon`, `authenticated`, `service_role` as reserved roles in `authz`.
- Map the request principal to one automatically: no/blank subject → `anon`;
  valid user token → `authenticated`; service-secret token → `service_role`
  (bypasses RLS like a superuser, but distinct and auditable).

### B4 — Role-scoped policies (MUST)
- Extend the policy grammar: `CREATE POLICY <name> ON <t> FOR <op> TO <role,...>
  USING (...) [WITH CHECK (...)]`. Persist the target-role set in the catalog policy
  blob (no FORMAT_VERSION bump — same place `rls_policy` lives).
- `apply_rls` only AND-injects a policy when the caller's effective roles intersect
  the policy's target set; policies with no `TO` default to all callers (back-compat).

### B5 — Column-level security (SHOULD, P1)
- This is the parked **item 112**. Fold its execution here since it shares the
  `apply_rls`/enforcement path; keep its own file as the detailed touch-map.

## Touch-points
- `src/sql/logical.rs` — `Expr::AuthUid`/`AuthClaim`, substitution walkers, role
  gate in `apply_rls`/`apply_rls_skip_current_user`.
- `src/sql/executor.rs` / `ExecCtx` — carry auth principal + verified claims.
- `src/authz/mod.rs` — reserved roles, effective-role resolution, policy target-role
  persistence.
- `src/server/auth.rs` + `handlers.rs` — pass verified claims/subject into the
  exec context (the one coordination seam with Workstream A).

## Correctness requirements (gate before ship)
- Every new substitution **fails closed** (Null + warn), never `Bool(true)` — the
  item-110 regression is the standing warning here.
- `service_role`/superuser RLS-bypass must stay on the audited path (item 103).
- Claims only ever originate from a verified token; no injection from user SQL.

## Acceptance
- Policy `USING (tenant_id = auth.jwt()->>'tenant')` isolates two tenants' rows
  under two different tokens — integration test with count assertions.
- `auth.uid()` policy + role-scoped `TO authenticated` policy each proven with
  positive/negative cases; `anon` sees only `anon`-targeted policies.
- All existing RLS/`current_user` tests stay green; the item-110 LIMIT-coercion
  regression test still passes.
- Metrics/outcomes in `PROGRESS.md` per §6.
