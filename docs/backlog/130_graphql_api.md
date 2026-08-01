# Schema-derived GraphQL API (`POST /graphql`)

**Type:** Milestone
**Status:** ✅ SHIPPED (2026-08-01) — Workstream C4 (item 120/123), read-only v1

> `pg_graphql` analog for unidb, except this one also exposes **graph edge
> traversal** and **vector similarity** as first-class GraphQL fields, not
> just FK relationships — the reason to build it at all. unidb's
> differentiator is relational + graph + vector in one engine; GraphQL's
> nested-query model is the natural surface for that. Part of the
> [Supabase parity roadmap](120_supabase_parity_roadmap.md) (Wave 3, C4 —
> see [`123_auto_rest_api.md`](123_auto_rest_api.md)).

## Scope shipped

- **Schema-generation approach:** `async_graphql`'s `dynamic` API
  (`async_graphql::dynamic::Schema`), rebuilt from the live catalog
  (`Engine::table_defs()`) on every request — no compile-time macros, so the
  schema always reflects whatever tables/columns/FKs currently exist. New
  optional, `server`-feature-gated dependency (`dynamic-schema` feature
  only — GraphiQL/Playground/email-validator extras dropped).
- **Types + scalar fields:** one `Object` type per (non-internal,
  GraphQL-name-valid) table; one scalar field per column, typed
  `Int`/`Float`/`Boolean`/`String`/a custom no-validator `JSON` scalar/
  `[Float!]` (`VECTOR`).
- **Root query fields:** `<table>(<col>, <col>_neq/_gt/_gte/_lt/_lte/
  _like/_ilike/_in/_is_null, orderBy, limit, offset): [Type!]!` — the same
  filter-operator matrix `/rest/v1` (C1) exposes as `<col>=<op>.<value>`,
  spelled as distinct typed GraphQL arguments.
- **FK relationship fields, forward + reverse:** resolved purely from
  catalog FK metadata, generalizing C2's `resolve_relation` pattern
  (candidate-gathering reused verbatim; enumerated up front instead of
  resolved one name at a time, with an alias-collision disambiguation pass
  since a schema gets one chance to name a field, unlike C2's per-request
  400).
- **Graph edge-traversal field:** `edges(type: String, direction:
  EdgeDirection = OUT): [Edge!]!` on any table with a single `Int64`
  primary key — actual graph edges (`__edges__`), not just FKs, the
  differentiator this workstream exists for.
- **Vector `near` field:** a **root** `near_<table>`/`near_<table>_<col>
  (vector: [Float!]!, k: Int!): [Type!]!` field for any table with a
  `VECTOR` column — deliberately root-level rather than nested under a
  fetched row, since a similarity search has no row to be relative to
  (unlike `edges`, which is relative to one row's PK). Runs the same
  `WHERE NEAR(...)` predicate the SQL surface already executes.
- **Introspection:** always enabled (`__schema`/`__type`), for tooling/G4.

**Deferred / SHOULD-LATER (not built, per the original spec's own scope
note):** mutations, subscriptions, aggregations, cursor-based pagination,
GraphQL over the auto-OpenAPI, and combining `near`/`edges` with the root
field's filter/order/limit machinery.

## The critical requirement — per-field authorization

Every resolved field (root query, FK forward/reverse, `edges`, `near`) runs
a parameterized SQL statement through the *exact same* enforced path
`POST /sql`/`/rest/v1` use — `authorize_sql_as_principal` (table/column-grant
pre-check) + `execute_sql_params_as_principal` (RLS + bind + execute under
the caller's `AuthPrincipal`), both reused directly from
`server::rest_resource` (`run_stmt`/`append_where`/`ParsedOp`/`Filter`/
`quote_ident`/`table_ident`/`validate_column`/`parse_order_value` all made
`pub(super)` for this purpose). This module adds **no parallel
policy/enforcement engine**. A row RLS hides comes back `null`/omitted,
never leaked; an ungranted column denies that field (which, because every
generated field type is non-null, null-propagates to `"data": null` — the
same "whole request denied" outcome a `SELECT *` missing one grant produces
over `/rest/v1`).

**Column-grant-correct projection (a real bug caught by the RLS-parity
test, not a hypothetical):** an early implementation always ran
`SELECT * FROM <table>`, which meant a caller granted only `SELECT (id,
email)` was denied even a `{ accounts { id email } }` query — stricter than
`/rest/v1`'s `select=id,email`, a parity violation. Fixed by projecting
only the GraphQL sub-fields actually requested (`ctx.field().selection_set()`
→ `requested_projection`, looked up per-table via a `SchemaCatalog` carried
as schema `Data`), mirroring `/rest/v1`'s own `select=` column projection.

**Graph edges specifically:** `Engine::edges_from` (the lower-level `GET
/edges/from/:id` route) has no RLS/grant check at all — any valid JWT gets
any edges. That is fine for that pre-existing, narrowly-scoped route, but
would violate this module's fail-closed requirement for a general-purpose
traversal field. Since `__edges__` is an ordinary catalog table under the
hood, the GraphQL `edges` field instead runs a `SELECT ... FROM __edges__`
through the same enforced `run_stmt` path — a caller needs an explicit
`GRANT SELECT ON __edges__` to traverse edges via GraphQL at all, and any
RLS policy on `__edges__` applies too. **Stronger** enforcement than the
existing route, not a regression, and proven in
`tests/server_graphql.rs::graph_edges_field_returns_connected_rows`
(a non-superuser with no grant is denied identically to a direct `SELECT *
FROM __edges__` over `/sql`).

**Injection safety:** filter values are always typed GraphQL arguments
bound as `$n` parameters (reusing `rest_resource`'s `ParsedOp`/
`render_filter`/`append_where`); `NEAR`'s `vector`/`k` are SQL literals per
this engine's grammar (bind params aren't accepted in those positions) but
are formatted only from already-parsed `f32`/`i64` GraphQL scalar values,
never raw client text — mirrors how `rest_resource.rs` already formats
`LIMIT`/`OFFSET` as validated integers, not bind params. Identifiers
(table/column/field names) come only from the catalog-derived schema; an
unrecognized `orderBy` value is rejected via `validate_column`, never built
into SQL text.

## Touch-points

- New `src/server/graphql.rs` (~1000 lines): schema construction, filter/
  projection helpers, and every field resolver.
- `src/server/rest_resource.rs`: several helpers (`quote_ident`,
  `table_ident`, `append_where`, `parse_order_value`, `validate_column`,
  `run_stmt`, `Filter`/`ParsedOp`, `Relation`, `strip_id_suffix`) widened
  from private to `pub(super)` so `graphql.rs` can reuse them — no logic
  changed, no new enforcement surface.
- `src/server/error.rs`: `map_status` widened to `pub(crate)` so GraphQL
  errors carry the same machine-readable code (`PERMISSION_DENIED`, ...)
  `/sql`/`/rest/v1` do, as a `"<CODE>: <message>"` prefix.
- `src/server/mod.rs`/`router.rs`: `pub mod graphql;` + `POST /graphql`
  mounted under the same `require_jwt` layer as every other data-plane
  route.
- `Cargo.toml`: `async-graphql` (`dynamic-schema` feature only), gated
  behind the existing `server` feature.
- `docs/REST_API.md`: new C4 section (schema-generation rules, the
  graph/vector fields, per-field authorization, injection safety) + an
  error-table note that `/graphql` always returns HTTP 200 per the GraphQL
  convention.

## Non-goals / boundary

Generic catalog-derived schema only — no per-table business logic. No
engine/storage change; server-layer over the existing enforced query/NEAR/
edge execution. Read-only (no mutations in v1).

## Acceptance

- `cargo test --no-run` (default, no `server` feature) clean.
- `cargo build --all-features` / `cargo clippy --all-features --all-targets
  -- -D warnings` / `cargo fmt --all -- --check` all clean.
- Crash harness: 54/54 (no storage/engine changes — sanity-checked anyway).
- `tests/server_graphql.rs` — 7/7: (a) scalar-field query, (b) FK forward +
  reverse nesting, (c) graph `edges` traversal (+ direction + grant-denial
  parity with a direct `/sql` query), (d) vector `near` root field
  (nearest-2 correctness against 3 known vectors), (e) RLS + column-grant
  parity with `/sql` (row-for-row match under an RLS policy; identical
  `PERMISSION_DENIED` + null-propagated `data` on an ungranted column), (f)
  introspection (`__schema`/`__type`), (g) injection safety (malicious
  filter value treated as data; malicious `orderBy` identifier rejected,
  never reaches SQL text).
- Every pre-existing `server_rest`/`server_graph`/`server_authz` test still
  passes unchanged (the `rest_resource.rs`/`error.rs` visibility widenings
  changed no behavior).
- No `PROGRESS.md` entry (task scope excluded it, matching items 124–129's
  precedent) — see this file for the verification detail instead.
