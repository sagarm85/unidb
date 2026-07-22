# RLS + LIMIT crashes for every non-superuser caller (Text→Bool coercion bug)

**Type:** Improvement
**Status:** ✅ FIXED 2026-07-22 (branch `fix/item-110-rls-limit`) — root cause
below was one layer deeper than the filing's (excellent) analysis suggested:
not the item-38 coercion arms themselves, but **`current_user` being
destroyed before they ever ran.** See "Root cause & fix" at the end.

Found integrating unidb-studio's Table Editor with per-user login: a non-superuser
querying an RLS-protected table with `LIMIT` gets a `400 SQL_PLAN_ERROR` on every
single request. Since every paginated UI (the Studio's Table Editor included)
appends `LIMIT` for every page, **RLS-protected tables are currently unusable in
any list/grid view for non-superuser users** — the only thing that still works is
an un-limited `SELECT *`.

## Repro (clean, from a fresh table, reproduced twice independently)

```sql
-- as a superuser:
CREATE TABLE repro_test (id INT, owner TEXT);
INSERT INTO repro_test (id, owner) VALUES (1, 'alice');
INSERT INTO repro_test (id, owner) VALUES (2, 'zzz_unrelated');
GRANT SELECT ON repro_test TO pm;              -- pm has alice as a member
CREATE POLICY repro_owner_only ON repro_test FOR SELECT USING (owner = current_user);
```

```
-- as alice (non-superuser, dev-login token):
SELECT * FROM repro_test;              -- OK: 1 row (correctly RLS-filtered)
SELECT * FROM repro_test LIMIT 10;     -- FAILS, every time:
```
```json
{"error":"SQL planning error: cannot coerce text 'alice' to boolean for comparison","code":"SQL_PLAN_ERROR"}
```

The error text is dynamic — it names whichever `current_user` is actually
calling (reproduced with both `alice` and `bob` on a separate table, each
producing their own name in the message), so the RLS substitution itself is
running; something downstream mishandles the result once `LIMIT` is present.

## What's ruled out

- **Not a stale/cross-user cache issue.** Reproduced with a brand-new, never-
  before-run SQL string (`SELECT * FROM repro_test WHERE budget > 0 LIMIT 10`
  on a different, already-populated table) — first execution ever, immediate
  failure. So this isn't item 96's plan cache returning another user's plan.
- **Not policy corruption.** `SELECT * FROM unidb_catalog.policies` shows the
  stored policy unchanged: `using_expr = "owner = current_user"`, never a
  literal username.
- **Not RLS-in-general.** `SELECT * FROM repro_test` (no LIMIT) with the same
  policy active correctly returns the filtered 1 row.
- **Not LIMIT-in-general.** `SELECT * FROM repro_test LIMIT 10` with no
  policy on the table at all works fine.
- **Superusers are unaffected** — `SELECT ... LIMIT` as a superuser on the
  same table returns all rows correctly (item 103's bypass still holds).

So the trigger is specifically: **non-superuser + a `current_user`-referencing
policy + `LIMIT` present**, on an otherwise completely ordinary query.

## Where I'd start looking

`src/sql/executor.rs`'s `compare()` — the error comes from `parse_bool_text`,
reached via the item-38 (implicit param coercion) arms:

```rust
(Literal::Text(s), Literal::Bool(r_b)) => { let a = parse_bool_text(s)?; ... }
(Literal::Bool(l_b), Literal::Text(s)) => { let b = parse_bool_text(s)?; ... }
```

Something is comparing a `Literal::Bool` against the RLS-substituted
`current_user` text literal directly, instead of the intended
`owner = current_user` text/text comparison. `repro_test` has no boolean
column at all, so this Bool operand isn't coming from the data.

Structural clue: `LogicalPlan::Select { table, projection, predicate }` (the
fast concurrent-read path `ReadHandle::execute_sql_as` — item 103's own fix —
operates on) has **no `limit` field whatsoever**. A trivial single-table
`SELECT ... LIMIT n` must therefore route through `LogicalPlan::Query(QuerySpec)`
(the Phase-4 executor, `src/sql/query_exec.rs`) instead of the simple-Select
fast path, the moment `LIMIT` is present — taking a completely different RLS
application route (`apply_rls_from` / `apply_rls_into_qexpr`, the same
functions the item-19 subquery-RLS fix touched) than the one item 103 fixed
and tested. My best guess: the `QuerySpec` path's LIMIT handling and the
`current_user` substitution/rewrite are interacting badly — possibly a
boolean "has-more"/"exec-time-budget-remaining"-style flag from the LIMIT
machinery ending up in the same expression slot as the substituted predicate.
I did not chase it into `query_exec.rs` itself — didn't want to guess at a fix
for a path this security-sensitive without being sure.

## Why this is more than a crash

Beyond breaking every paginated view, a bug in *which* value ends up compared
against what — this close to the RLS rewrite path — is exactly the class of
bug that in a different shape could leak rows instead of erroring loudly. It
errored loudly here, which is the safe failure mode, but the RLS+LIMIT
interaction should get a hard look for correctness, not just the crash fixed.

## Acceptance

- [ ] `SELECT * FROM t LIMIT n` as a non-superuser with a `current_user`
      policy returns the correctly RLS-filtered rows (not an error, not
      unfiltered rows).
- [ ] Add a regression test: RLS policy + LIMIT, non-superuser caller,
      asserting the row *count* is right (catches a future silent-bypass
      regression, not just the crash).
- [ ] Existing `tests/item103_authz_bypass.rs` extended to cover a LIMIT
      variant of each existing case.


## Root cause & fix (2026-07-22)

The filing's structural clue was exactly right: `LIMIT` forces
`LogicalPlan::Query(QuerySpec)`. The chain:

1. `substitute_current_user_in_plan` had **no arm for
   `LogicalPlan::Query`** (`_ => {}`) — both substitution passes no-op'd.
2. `apply_rls` injects the policy into the QuerySpec, which **eagerly
   converts** the policy `Expr` → `QExpr` (`qualify_policy`)…
3. …whose fallback rewrote an unsubstituted `Expr::CurrentUser` into
   **`Literal::Bool(true)`**. The policy became `owner = TRUE` →
   `compare(Text("alice"), Bool(true))` → the item-38 arm's
   `parse_bool_text("alice")` → the exact reported error.

Everything in the repro matrix follows: superusers skip policy injection
(fine); no-LIMIT routes to `LogicalPlan::Select` which keeps `Expr` form
until exec (fine); LIMIT-no-policy has no `CurrentUser` anywhere (fine).

**The "hard look for correctness" the filing asked for found a real leak
shape:** in any policy position where `Bool(true)` type-checks, the old
fallback silently WEAKENED the policy instead of erroring. It fail-crashed
here only by the luck of a Text column.

### Fix

- `apply_rls(plan, catalog, user)`: the caller's identity is now a
  parameter, and for the QuerySpec path `current_user` is substituted into
  the policy `Expr` **at injection time**, before the conversion can
  destroy it (Select/Update/Delete arms unchanged — their later plan-level
  substitution already worked). Prepared/bound path passes `None`.
- The conversion fallback **fails closed**: an unresolved `current_user`
  becomes `Literal::Null` (policy not-true for every row) + a warning log —
  never `Bool(true)`. Other permissive no-op shapes unchanged.
- Regression tests (`tests/item110_rls_limit.rs`, 5): the filed repro
  (RLS+LIMIT returns the filtered rows, count-asserted per the acceptance's
  silent-bypass guard), LIMIT-clamp, superuser+LIMIT, no-policy+LIMIT,
  ORDER BY/GROUP BY routing shapes, and two-user isolation through the
  same cached SQL.

Verification: full suite 70 binaries green · crash harness 54/54 ·
clippy/fmt clean.
