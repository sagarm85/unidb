# Auto-generated data API — PostgREST-style resource routes (Workstream C)

**Type:** Milestone
**Status:** NOT STARTED

> Gives clients a schema-derived, resource-oriented REST API
> (`/rest/v1/<table>?col=eq.val`) instead of only the raw `POST /sql` surface —
> the PostgREST role in Supabase. Part of the
> [Supabase parity roadmap](120_supabase_parity_roadmap.md) (Wave 1; C1 is P0).
> Enforces the *existing* RLS/grant engine, so it has no hard dependency on A or B
> and can start in parallel (it gets richer automatically once B lands).

## Problem

Today every client action goes through `POST /sql` (see unidb-studio, which builds
its own SQL for every panel). A Supabase-style client expects to `GET
/rest/v1/users?id=eq.1&select=id,name` without composing SQL, with filtering,
ordering, pagination, and FK embedding derived from the catalog. The engine already
has the introspectable catalog (Milestone 18) and cursor paging (item 30/cursor) to
build this on.

## Scope

### C1 — Resource routes + filter operators (MUST, P0)
- `GET /rest/v1/<table>` → SELECT; `POST` → INSERT; `PATCH` → UPDATE; `DELETE` →
  DELETE, each translated to the existing logical plan (NOT string-built SQL —
  build the `LogicalPlan`/`QuerySpec` directly to avoid an injection surface).
- Query-param operators: `col=eq.v`, `neq`, `gt/gte/lt/lte`, `like/ilike`, `in.(…)`,
  `is.null`. `select=` column projection. `order=col.asc/desc`. `limit`/`offset`
  and a `Range` header for keyset paging (reuse the cursor infra).
- **Authorization is not re-implemented**: the generated plan runs through the same
  `apply_rls` + grant checks as `/sql`, under the caller's token. RLS/grants are the
  single source of truth — this route is only a plan builder.

### C2 — Embedded resource expansion (SHOULD)
- `select=id,name,orders(id,total)` → resolve the FK from the catalog and emit a
  nested result via the existing join executor. Read-only in v1.

### C3 — OpenAPI / API-docs generation (SHOULD)
- `GET /rest/v1/` returns an OpenAPI 3 document generated from the catalog
  (tables, columns, types, PK/FK). Feeds unidb-studio's API-docs panel (G4) and any
  codegen.

### C4 — GraphQL (COULD, P2, Wave 3)
- A schema-derived GraphQL endpoint (`pg_graphql` analog). Separate effort; listed
  here for completeness, spun out to its own `NN_…` file when started.

## Touch-points
- `src/server/router.rs` — mount the `/rest/v1` sub-router under the same
  `require_jwt` layer as the data plane.
- New `src/server/rest_resource.rs` — param→`LogicalPlan` translation + result
  shaping. Reuses `sql::logical`, `sql::executor`, cursor, and `information_schema`.
- `src/server/dto.rs` — response shapes; `src/server/error.rs` — map engine errors
  to REST status codes consistently with the existing contract.
- No engine-core changes; this is a server-layer feature over existing plans.

## Non-goals / boundary
- Honors the Milestone-18 boundary spirit: this is a *generic* schema-derived API,
  not hand-written app-shaped endpoints. It complements `/sql`, it does not replace
  it, and it must not grow bespoke per-table business logic.

## Acceptance
- CRUD over `/rest/v1/<t>` with the full operator matrix, proven against a
  differential SQL equivalent; RLS/grant enforcement identical to `/sql` (a
  restricted user sees the same rows both ways) — integration tests.
- Injection safety proven: params never string-concatenated into SQL; a malicious
  `col`/value cannot escape the plan builder.
- OpenAPI doc validates and round-trips the catalog schema.
- Metrics/outcomes in `PROGRESS.md` per §6.
