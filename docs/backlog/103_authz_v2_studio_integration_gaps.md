# AuthZ v2 — gaps found wiring the Studio's Auth tab against a live server

**Type:** Improvement
**Status:** NOT STARTED

Found while building unidb-studio's Auth tab (Roles/Grants/Policies/Preview) against
a live `unidb-server` on `main` (2026-07-20, post item-24 Z1/Z2/Z4/Z5/Z6 merge —
PRs #152, #163, #166, #167, plus the Z6 auth/preview commits). Every read/write path
the Studio needed was exercised end-to-end via curl against a running server; two
correctness/doc gaps surfaced. Filed as one item per the reporting session's request
— split into separate PRs when picked up if that's cleaner.

**Update 2026-07-20, post PR #168 (item-24 R-a/R-b + item 100):** re-ran the exact
Gap 1 repro below against the server after #168 landed — **still reproduces
identically** (both rows still come back empty for the superuser and no-`sub`
tokens). This is a *different* bug from what #168 fixed: #168's R-a closed a
write-side escape (`UPDATE ... SET owner='bob'` letting a row leave its own
policy — a regular user seeing/writing *too much*); Gap 1 here is a superuser
seeing *too little* on plain reads, the documented bypass never firing. Gaps 2
and 3 (doc staleness) are also both still present on `main` as of #168. Numbered
103 — `101` (group-commit dwell window, PR #170) and `102` (index-only scan,
PR #169) were both filed and shipped on `main` after this branch's fork point;
renumbered to 103 to avoid collision. Content otherwise unchanged.

## Gap 1 — superuser / no-`sub` caller does not bypass `current_user` RLS policies

`docs/REST_API.md` states (Row-level security section, `current_user` subsection):

> Superusers and the embedded API path (`sub` absent) bypass `current_user`-containing
> policies entirely.

This is not what happens today. Reproduced directly against a live server:

```
CREATE TABLE demo_orders (id INT, owner TEXT, amount INT);
INSERT INTO demo_orders VALUES (1, 'test_user', 100);
INSERT INTO demo_orders VALUES (2, 'someone_else', 200);
CREATE POLICY orders_owner_only ON demo_orders FOR SELECT USING (owner = current_user);

-- as the superuser dev token (is_superuser = true):
POST /sql {"sql": "SELECT * FROM demo_orders"}
  -> {"rows": []}                      -- expected: both rows (bypass), got: none

-- as a freshly minted JWT with NO `sub` claim at all (the documented
-- "embedded API path" / implicit-superuser case):
POST /sql {"sql": "SELECT * FROM demo_orders"}
  -> {"rows": []}                      -- same result, same gap
```

Both the named-superuser path and the no-`sub` path evaluate `owner = current_user`
as if `current_user` were present and non-matching (most likely resolving to an
empty string or NULL) instead of skipping the policy rewrite entirely for these
callers, as documented. Net effect: once *any* `current_user`-referencing policy
exists on a table, superuser/no-`sub` queries against that table via plain
`POST /sql` silently return fewer rows than they should — a correctness bug, not
just a docs bug, since a superuser reading their own data via the SQL Editor (or
any admin tool) gets silently filtered results with no error.

Note this doesn't affect `POST /auth/preview` — that route always evaluates as the
named `as_role`, never through the superuser-bypass path, so its results were
correct in every case tested.

This is exactly the milestone's own unchecked acceptance item from
`24_authz_v2_policies.md`:

```
- [ ] Superuser/bootstrap semantics and `BYPASSRLS` equivalent — explicit, audited.
```

**Suggested fix shape:** in the RLS rewrite (`apply_rls` / wherever `current_user` is
substituted), check `is_superuser(caller)` (and the `sub`-absent case) *before*
substituting `current_user`, and skip the AND-rewrite for that policy entirely for
those callers — matching the doc's stated contract — rather than substituting a
placeholder value that happens to satisfy no row.

## Gap 2 — stale `CREATE ROLE ... SUPERUSER` example in REST_API.md

`docs/REST_API.md`'s per-user authorization section shows:

```sql
CREATE ROLE analyst;
CREATE ROLE admin SUPERUSER;   -- <- does not match the real grammar
```

The actual grammar in `src/authz/mod.rs` (and its own header comment) only accepts
`SUPERUSER` on `CREATE USER`:

```
CREATE USER <name> [SUPERUSER]        DROP USER <name>
CREATE ROLE <name>                    DROP ROLE <name>
```

`parse_auth_stmt`'s `("CREATE", "ROLE")` arm builds `AuthStmt::CreateRole(String)` —
no superuser field exists on that variant. Roles are pure permission groups;
`SUPERUSER` is a user attribute only. The doc example should be
`CREATE USER admin SUPERUSER;` (or dropped as a role example entirely). Low
severity (docs-only) but worth fixing alongside Gap 1 since both were found the
same way — a consumer (the Studio) building directly against the documented
contract.

## Minor — Gap 3, doc completeness

`docs/REST_API.md`'s "Catalog virtual relations" paragraph (end of the per-user
authorization section) only lists:

```sql
SELECT * FROM unidb_catalog.roles;
SELECT * FROM unidb_catalog.grants;
SELECT * FROM unidb_catalog.policies;
```

`unidb_catalog.role_members` and `unidb_catalog.users` (item-24 Z4, PR #166) are
fully implemented (`information_schema.rs`'s `RELATIONS` list, confirmed via curl)
but aren't mentioned in this specific paragraph — purely a doc omission, the code
and its own comments are correct.

## Acceptance

- [ ] Superuser and no-`sub` callers see unfiltered rows on tables with
      `current_user`-referencing policies, verified with the exact repro above
      (2-row table, one policy, both bypass paths).
- [ ] `docs/REST_API.md`'s `CREATE ROLE ... SUPERUSER` example corrected.
- [ ] "Catalog virtual relations" paragraph lists all five relations
      (`roles`, `grants`, `policies`, `role_members`, `users`).
