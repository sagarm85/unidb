# AuthZ v2: superuser RLS bypass fix + doc gaps (item 103)

**Type:** Improvement
**Status:** SHIPPED (→ PROGRESS.md "Item 103 — AuthZ v2: superuser RLS bypass")

Found while building unidb-studio's Auth tab (Roles/Grants/Policies/Preview) against
a live `unidb-server` on `main` (2026-07-20, post item-24 Z1/Z2/Z4/Z5/Z6 merge —
PRs #152, #163, #166, #167, plus the Z6 auth/preview commits). Every read/write path
the Studio needed was exercised end-to-end via curl against a running server; two
correctness/doc gaps surfaced. Filed as one item per the reporting session's request.
Numbered 103 — `101` (group-commit dwell window, PR #170) and `102` (index-only scan,
PR #169) were both filed and shipped on `main` after this branch's fork point;
renumbered to 103 to avoid collision.

## Problem

Three gaps were found during Studio integration testing:

### Gap 1 — Superuser / no-`sub` RLS bypass (correctness bug)

**Confirmed live against a running server:** superuser and no-`sub` (embedded API)
callers were NOT bypassing `current_user`-referencing RLS policies on the
concurrent read path (`ReadHandle::execute_sql`). The `ReadHandle` was calling
`apply_rls` unconditionally with no user context, so a `CurrentUser` node in a
policy expression was never substituted — it evaluated to `Null`, making the
filter `owner = Null` → always false → 0 rows returned even for superusers.

On the writer path (`execute_sql` / `execute_sql_inner`), `apply_rls_skip_current_user`
was already in use, which correctly skips policies containing `CurrentUser`. But
the concurrent read handle, the session-based path, and the `post_batch_sql` handler
all lacked user context.

**Root cause paths:**
- `ReadHandle::execute_sql` → `apply_rls` (no user context)
- `post_sql` session path → `execute_sql(xid, sql)` → no user identity passed
- `post_sql` one-shot path → `execute_sql(xid, sql)` same issue
- `post_batch_sql` → `execute_sql_read` / `execute_sql` (no user context)

### Gap 2 — Stale doc example

`docs/REST_API.md` showed `CREATE ROLE admin SUPERUSER` which is invalid syntax —
`SUPERUSER` is a `CREATE USER` attribute only; `CREATE ROLE` does not accept it.

### Gap 3 — Missing catalog virtual relations

`docs/REST_API.md` listed only `roles`, `grants`, `policies` in the "Catalog virtual
relations" paragraph. The `role_members` and `users` virtual tables that ship as
part of item-24 Z4/Z5 were absent.

## Fix (shipped 2026-07-20, PR #173)

- `ReadHandle` gained an `Arc<RoleStore>` field and a new `execute_sql_as(user, sql)`
  method that correctly applies the same `skip_current_user_policies` gate as
  `execute_sql_inner_as` on the writer path.
- `Engine::read_handle()` passes `Arc::clone(&self.authz)` to `ReadHandle::new`.
- `EngineHandle` gained `execute_sql_read_as(user, sql)` which delegates to
  `ReadHandle::execute_sql_as`.
- `post_sql` and `post_batch_sql` handlers updated to pass the JWT user identity
  through to the read and write paths so superusers and the no-sub path get the
  correct bypass.
- `docs/REST_API.md` corrected for both doc gaps.
- Three tests in `tests/item103_authz_bypass.rs`: named-superuser bypass,
  no-sub bypass, regular-user still filtered.

## Repro (archived)

```sql
CREATE TABLE demo_orders (id INT, owner TEXT, amount INT);
INSERT INTO demo_orders VALUES (1, 'test_user', 100);
INSERT INTO demo_orders VALUES (2, 'someone_else', 200);
CREATE POLICY orders_owner_only ON demo_orders FOR SELECT USING (owner = current_user);

-- as the superuser dev token (is_superuser = true):
POST /sql {"sql": "SELECT * FROM demo_orders"}
  -> {"rows": []}                      -- expected: both rows (bypass), got: none (BUG)

-- as a freshly minted JWT with NO `sub` claim at all (the documented
-- "embedded API path" / implicit-superuser case):
POST /sql {"sql": "SELECT * FROM demo_orders"}
  -> {"rows": []}                      -- same result, same gap (BUG)

-- after fix: both callers return both rows
```

## Acceptance (all ✅)

- [x] Superuser and no-`sub` callers see unfiltered rows on tables with
      `current_user`-referencing policies (3 tests in `tests/item103_authz_bypass.rs`).
- [x] `docs/REST_API.md`'s `CREATE ROLE ... SUPERUSER` example corrected.
- [x] "Catalog virtual relations" paragraph lists all five relations
      (`roles`, `grants`, `policies`, `role_members`, `users`).
