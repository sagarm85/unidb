# information_schema visibility should follow existing table grants

**Type:** Improvement
**Status:** NOT STARTED

Raised while wiring unidb-studio's Table Editor sidebar up to per-user login: a
user with full CRUD grants on a table (`SELECT`/`INSERT`/`UPDATE`) still gets
`403 PERMISSION_DENIED` reading `information_schema.tables` /
`information_schema.columns` unless *separately* granted access to those two
relations specifically.

```sql
GRANT SELECT, INSERT, UPDATE ON projects TO pm;   -- bob is a pm member
```
```
-- as bob:
SELECT * FROM projects;                            -- OK, correctly filtered by RLS
SELECT * FROM information_schema.columns;           -- 403 PERMISSION_DENIED
```

This means any client that needs to *discover* a table's structure before
querying it (a table-editor UI, a codegen tool, `\d tablename`-style
tooling) is blocked even for a user who can fully read and write the table's
data. Confirmed in `src/sql/information_schema.rs`: `tables_rows()` /
`columns_rows()` take `defs: &[&TableDef]` with no per-caller filtering at
all — every registered table is listed for anyone holding a grant on the
view itself, unconditionally.

## What Postgres does instead

`information_schema.tables`/`.columns` are security-invoker views filtered
by `has_table_privilege(current_user, table, 'SELECT')` (or any other
privilege) per row — so *having any privilege on a table is what makes it
show up*, with no separate schema-visibility grant to manage. Two useful
properties fall out of that: (1) it's zero extra grants to administer per
role, and (2) a user only ever sees the *shape* of tables they can already
touch — unlike a blanket `GRANT SELECT ON information_schema.tables`, which
(per the current unfiltered implementation above) would reveal every table
in the database, including ones the grantee has no data access to at all.

## Suggested shape

Make `tables_rows()`/`columns_rows()` authz-aware the same way `roles_rows()`/
`grants_rows()` already are (they take `&RoleStore`): filter each `TableDef`
by whether the caller holds *any* privilege on it (or is superuser / open
mode), mirroring `RoleStore::has_privilege`. No new grant vocabulary needed —
this makes an existing grant do double duty, matching the mental model most
users bring from Postgres/Supabase ("if I can query it, I can see its
columns").

## Acceptance

- [ ] A user with any grant on table `t` can read `t`'s own row(s) from
      `information_schema.tables`/`.columns` without a separate grant.
- [ ] A user with zero grants on table `t` does not see `t` in either view
      (today they'd need a blanket grant that reveals every table).
- [ ] Superuser / open-mode behavior unchanged (sees everything).
