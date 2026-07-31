# Auto-generated data API — PostgREST-style resource routes (Workstream C)

**Type:** Milestone
**Status:** C1 + C3 SHIPPED (`742b355`, 2026-07-31); C2 SHIPPED 2026-07-31 (forward + reverse
embedding); C4 GraphQL not started (deferred, P2/Wave 3)

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

### C2 — Embedded resource expansion (SHOULD) — SHIPPED 2026-07-31
- `select=id,name,orders(id,total)` → resolve the FK from the catalog and emit a
  nested result. Read-only in v1 (`GET` only).
- Shipped **both** forward (many-to-one, MUST) and reverse (one-to-many, SHOULD) —
  reverse turned out to be a modest addition once forward's relation-resolution
  and stitching machinery existed, so both landed together rather than deferring
  reverse to a follow-up.
- Implementation: **stitch, not JOIN** — the base query runs as before, then one
  additional parameterized `SELECT ... WHERE <join_col> IN ($1,...)` per embed
  (keyed by the distinct non-NULL join values collected from the base result),
  through the identical `run_stmt` enforced path (`authorize_sql_as_principal` +
  `execute_sql_params_as_principal`) as everything else in `rest_resource.rs`.
  Nesting happens in Rust after both enforced queries return. Chosen over a
  generated JOIN because it keeps RLS/grant enforcement on the embedded table
  trivially correct (it's the exact same enforced single-table query path,
  never a new join-aware enforcement surface) — the explicit tradeoff the C2
  scope note called out, resolved in the "usually simpler, keeps enforcement
  trivially correct" direction.
- Relationship resolution (`resolve_relation` in `rest_resource.rs`) is purely
  catalog-derived: matches the embed name against the base table's FK columns
  (forward) or another table's FK columns targeting the base table (reverse),
  including a `_id`-suffix-stripped alias (`customer_id` → `customer`).
  Zero matches → `400 UNKNOWN_RELATIONSHIP`; more than one → `400
  AMBIGUOUS_RELATIONSHIP` (never a silent first-match). Composite (multi-column)
  FKs are out of scope for v1 embedding.
- Tests: `tests/server_rest.rs` — forward embed, filter+embed combo, reverse
  array, unknown/ambiguous relationship (400), RLS+column-grant parity on the
  embedded table (a restricted caller's disallowed nested row comes back
  `null`/omitted, never leaked; an ungranted embedded column denies the whole
  request exactly like a direct request for that column would), and injection
  safety (a malicious filter value and a malicious relationship name are both
  proven to be treated as data/never matched, never built into SQL text).

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
  restricted user sees the same rows both ways) — integration tests. ✅ (C1)
- Injection safety proven: params never string-concatenated into SQL; a malicious
  `col`/value cannot escape the plan builder. ✅ (C1); extended to embed names and
  embed-query values (C2) — a malicious relationship name is matched against
  catalog metadata only (never reaches SQL text) and simply fails to resolve.
- OpenAPI doc validates and round-trips the catalog schema. ✅ (C3)
- Embedded resources (C2): FK relationship resolved from catalog metadata only
  (no per-table logic); RLS/grants enforced identically on the embedded table;
  ambiguous/unknown relationships rejected with a clear 400. ✅ (C2)
- Metrics/outcomes in `PROGRESS.md` per §6.
