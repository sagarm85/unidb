# GraphQL mutations

**Type:** Improvement
**Status:** SHIPPED (2026-08-01, PR #235) — `Mutation` root (`insert_/update_/delete_<t>`)
routed through the same `run_stmt`/`run_stmts` → `execute_sql_params_as_principal` path
`/rest/v1`/`/sql` use; RLS + `WITH CHECK` + column grants inherited (parity-tested),
`RETURNING` projects requested sub-fields only. Crash 54/54. See the file for detail.

> Supabase-parity gap (item 120 / 130 follow-up). unidb's GraphQL API (C4, item
> 130, `POST /graphql`) shipped **read-only v1** — a `Query` root only. Supabase's
> `pg_graphql` also exposes `insert`/`update`/`delete` mutations. This adds a
> `Mutation` root so the GraphQL surface is write-capable, at full ACID/RLS
> parity with `/rest/v1` and `/sql`.

## Why this is safe by construction (ACID/RLS/perf)

Every mutation resolver routes writes through the **exact same enforced path**
the REST mutators (`src/server/rest_resource.rs`) and `/sql` already use:
`run_stmt` → `execute_sql_params_as_principal` (parameterized `$n` binds, RLS +
`WITH CHECK` + table/column grants evaluated under the caller's `AuthPrincipal`,
executed as one engine statement/transaction). There is **no new write path,
no new SQL string-building, and no engine change** — so ACID, MVCC, and the
crash harness are untouched by construction (54/54 stays green), and RLS/grant
enforcement is inherited, not re-implemented. This mirrors exactly how item 130
proved its read side had "zero parallel enforcement engine."

## Scope

Extend `src/server/graphql.rs`'s dynamic schema (`async_graphql::dynamic`).
Today it is `Schema::build("Query", None, None)`; add a `Mutation` root object
(`Schema::build("Query", Some("Mutation"), None)` + register the object) with,
per eligible table `t` (same eligibility filter as the query side — skip
`__…__` internal tables and GraphQL-reserved names):

- **`insert_<t>(values: JSON!): <T>`** — single-row insert; returns the inserted
  row projected to the requested sub-fields. (A bulk `[JSON!]` variant is a nice-
  to-have; single-row is the v1 requirement.) Maps to the same INSERT the REST
  `POST /rest/v1/<t>` builds.
- **`update_<t>(<filter args>, set: JSON!): [<T>!]`** — updates rows matching the
  same filter arguments the query side already generates (`col_eq`, `col_gt`,
  …), returns the updated rows. Maps to REST `PATCH`.
- **`delete_<t>(<filter args>): [<T>!]`** — deletes matching rows, returns the
  deleted rows (RETURNING). Maps to REST `DELETE`.

Reuse the existing helpers already widened `pub(super)` in item 130
(`run_stmt`, `append_where`, `quote_ident`, `table_ident`, `ParsedOp`, the
`requested_projection` machinery for per-field grant correctness) — do not fork
them.

## Correctness details (learn from item 130)
- **Projection = requested sub-fields only** (the C4 over-grant bug): the
  RETURNING / post-write projection must request exactly the fields the mutation
  selection set asks for, so a caller only needs grants on what they read back —
  not `SELECT *`.
- **Values/`set` are data, never identifiers**: column *names* from the JSON keys
  are catalog-validated + quoted (same as REST); *values* go through `$n` binds.
  A key that isn't a real column → the same error REST returns, not a silent drop.
- **`WITH CHECK` / RLS on write** is enforced by the shared path — add a test
  that an RLS-violating insert/update is rejected identically to `/rest/v1`.
- Errors carry the same machine-readable code mapping as the query side
  (`api_err_to_gql`).

## Acceptance
- `insert_/update_/delete_<t>` work end-to-end via `POST /graphql`, returning the
  requested projection.
- **RLS + column-grant parity test vs `/sql`/`/rest/v1`**: an insert/update that
  RLS/`WITH CHECK` forbids is rejected; a projection over an ungranted column is
  denied — matching the REST path exactly (extend `tests/server_graphql.rs` or a
  new `tests/item133_graphql_mutations.rs`, `#![cfg(feature = "server")]` at top).
- Every pre-existing `server_graphql`/`server_rest`/`server_authz` test unchanged.
- **Crash harness 54/54 unchanged.**
- `cargo test --no-run` (no features) + `clippy --all-features --all-targets
  -D warnings` + `fmt` clean.
- `docs/REST_API.md` C4 section (mutations added), `README.md` GraphQL bullet,
  and `docs/backlog/130_graphql_api.md` "deferred: mutations" note + this file's
  Status flipped on merge.

## Non-goals (v1)
- No GraphQL **subscriptions** (that would ride the realtime SSE work, item 132 /
  a later item — flag, don't build here). No transaction batching across multiple
  mutation fields in one request beyond async-graphql's default sequential
  Mutation execution. No upsert/`on conflict` sugar (a follow-up if asked).
