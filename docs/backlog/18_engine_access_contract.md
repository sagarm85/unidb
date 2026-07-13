# Engine access & introspection contract — "build your app on the engine"

**Type:** Milestone
**Status:** NOT STARTED

> **Vision.** unidb is an *engine*, like Postgres. It does **not** ship
> application-shaped REST resources. It ships a documented **access + query +
> introspection surface**; every application connects over that surface (embed /
> attach client / access-token URL) and builds *its own* REST/UI on top. The
> `unidb-studio` web UI is the **first consumer** and the forcing function for
> this doc.
>
> This backlog defines and documents that surface so an end user can *"read it and
> build around it, like Postgres."* It is a spec/plan; metrics and closeout land
> in `PROGRESS.md` per `CONVENTIONS.md`.

---

## Why now / positioning

- **Studio surfaced the gap.** Building a Supabase-style console (schema
  visualizer/ERD, table+column search, column filters, DDL viewer) needs
  first-class **introspection**: primary keys, foreign keys, indexes, and object
  DDL. Today only a flat table/column list exists (`GET /tables`, item 12), so the
  studio has to *infer* relationships from column-name heuristics — honest but
  wrong in the general case.
- **The fix is NOT more app endpoints.** The instinct to add `/schema`,
  `/relationships`, `/ddl`… REST routes to the server is the engine doing the
  *application's* job (that is what Supabase/PostgREST are — an application layer
  over Postgres). `REST_API.md` already states the design intent: the server is
  *"a thin HTTP wrapper over the embedded `Engine` … not a resource-oriented,
  auto-generated API in the PostgREST sense."* This milestone doubles down on
  that.
- **The right lever: a documented, queryable catalog.** Postgres exposes
  everything a tool needs through `information_schema` / `pg_catalog` — you
  `SELECT` from catalog relations over the *same* query surface. unidb should do
  the same. Then any application (REST, CLI, notebook, the studio) builds on one
  documented surface, and the engine never learns the word "schema visualizer."

## Non-goals

- **No PostgREST-style auto-generated REST** in the engine or `unidb-server`.
- **No application-specific endpoints** (`/schema`, `/relationships`, …). These
  are explicitly *superseded* by the SQL-queryable catalog in Epic C. (The
  studio-side `docs/SCHEMA_API_PROPOSAL.md` in the `unidb-studio` repo was the
  interim sketch; it is retired by this milestone.)
- **No auth/identity system.** Token *verification* only, as today (`auth.rs`,
  HS256 verify-only). Issuing tokens stays out of scope.

## Consumers / traceability

Every capability below exists because a concrete application needs it. First
consumer is the studio; the surface must generalize beyond it.

| Application need (studio) | Engine capability this milestone defines |
|---|---|
| Schema visualizer / ERD edges | Epic C — foreign-key + primary-key catalog relations |
| Node PK/FK badges | Epic C — `key_column_usage`, `table_constraints` |
| "View DDL" per table | Epic C — `object_ddl(name)` (or reconstructable metadata) |
| Table list + search | Epic C — `tables`, `columns` (supersedes flat `/tables`) |
| Column filters | Epic B — parameterized SQL (`$n`) — already shipped, documented here |
| Record browser paging | Epic D — keyset/cursor semantics + result column metadata |
| "Connect" from any app | Epic A — access-token URL + connect contract |

---

## Access model (how an application reaches the engine)

Three supported access paths, one query/catalog surface across all of them:

| Path | Crate / entry | For | Notes |
|---|---|---|---|
| **Embed** | `unidb-embed` (`Engine` as a library) | in-process apps (Rust) | zero network; direct API |
| **Attach** | `unidb-attach` (Rust client) | out-of-process apps | wire protocol to a running instance |
| **Server** | `unidb-server` over an **access-token URL** | any language / HTTP | thin `/sql` wrapper; **only** path a browser can use |

**Browser caveat (must be documented, not hidden).** A browser cannot open a raw
socket, so a browser SPA either (a) talks to the `unidb-server` access-token URL
over HTTPS/WebSocket (generic query surface — *not* app resources), or (b) runs
its own backend-for-frontend that embeds/attaches and serves app-shaped REST to
its own frontend. Either way the boundary holds: **the engine stops at a generic
query + catalog surface; the application owns its REST.**

---

## Epics & stories

Stories carry acceptance criteria (AC). Priority is MoSCoW. "Ships when" points at
the check that closes it. Epic C is the heart of the milestone.

### Epic A — Access & auth contract (document + firm up)

- **A1 — Access-token URL format.** *As an app developer, I can point a client at
  a single access-token URL so I connect without embedding secrets in code.*
  Priority: **Must**.
  - AC: A documented URL/DSN form (e.g. `unidb://<token>@<host>:<port>/<db>` for
    attach, and the `https://<host>?token=` / `Authorization: Bearer` form for the
    server) with token placement, TLS expectations, and default port.
  - AC: One db-per-server vs. multi-db addressing is stated explicitly.
- **A2 — Session & transaction lifecycle.** *As an app developer, I understand
  when a connection is a session vs. a one-shot request.* Priority: **Should**.
  - AC: Documents auto-commit (no `X-Txn-Id`) vs. session (`X-Txn-Id`) semantics
    already in `REST_API.md`, and the embed/attach equivalents.

### Epic B — Query surface & type system (document what exists)

- **B1 — SQL surface reference.** *As an app developer, I know exactly which SQL I
  can send.* Priority: **Must**.
  - AC: Enumerates supported DDL/DML/SELECT (joins, aggregates, CTEs),
    transactions, and `EXPLAIN [ANALYZE]`, with the honest "not supported yet"
    list so builders don't guess.
- **B2 — Parameterized statements.** *As an app developer, I bind `$n` params so I
  can build filters/search safely.* Priority: **Must** (already implemented; the
  studio column-filters rely on it).
  - AC: `$1..$n` binding, type coercion rules, and injection-safety note are
    documented with examples.
- **B3 — Type system ↔ representation mapping.** *As an app developer, I know how
  each engine type appears in results (native + JSON).* Priority: **Must**.
  - AC: A table mapping every engine type to its JSON/wire representation
    (e.g. DECIMAL→string, TIMESTAMP→UTC string, JSON→nested value, NULL→null).
- **B4 — Error model.** *As an app developer, I can branch on stable error codes.*
  Priority: **Should**.
  - AC: The `{ error, code }` envelope + the full status/code table are the
    documented contract (extends the existing `error.rs` table).

### Epic C — System catalog (information_schema-style introspection) — CORE

Expose introspection as **relations you `SELECT` from over the normal query
surface** — no bespoke endpoints. Mirror `information_schema` naming for instant
familiarity (Postgres knowledge transfers); a compact native `unidb_catalog`
namespace may back it.

- **C1 — Tables & columns.** *As a tool builder, I can list tables and their
  columns with type/nullability/ordinal.* Priority: **Must**.
  - AC: `information_schema.tables` and `information_schema.columns` are
    queryable and documented (columns: name, data_type, is_nullable, default,
    ordinal_position).
  - AC: Supersedes the flat `GET /tables`; that route stays for back-comft but the
    catalog is the documented source of truth.
- **C2 — Primary keys & constraints.** *As a tool builder, I can identify each
  table's primary key and unique/check constraints.* Priority: **Must**.
  - AC: `table_constraints` + `key_column_usage` expose PK/unique/check with the
    columns participating, ordered.
- **C3 — Foreign keys / relationships.** *As a tool builder, I can enumerate real
  foreign keys to draw ERD edges — no name-heuristic guessing.* Priority: **Must**.
  - AC: `referential_constraints` (+ `key_column_usage`) yield
    `(from_table, from_columns[], to_table, to_columns[])`, ordered & parallel for
    composite keys.
  - AC: The studio's inferred/dashed edges become real/solid when this lands.
- **C4 — Indexes.** *As a tool builder, I can list indexes and their columns.*
  Priority: **Should**.
  - AC: `unidb_catalog.indexes` exposes index name, table, columns, uniqueness.
- **C5 — Object DDL.** *As a tool builder, I can fetch the authoritative
  `CREATE …` text for an object (the ⋮ → View DDL action).* Priority: **Should**.
  - AC: `unidb_catalog.object_ddl(<name>)` returns the engine's canonical DDL, or
    the milestone documents that DDL is reconstructable from C1–C4 and specifies
    the reconstruction rules.

**Worked example — everything the ERD needs, zero engine REST:**

```sql
-- tables + columns
SELECT table_name, column_name, data_type, is_nullable, ordinal_position
FROM   information_schema.columns
WHERE  table_schema = 'public';

-- foreign keys (drives the ERD edges + FK badges)
SELECT tc.table_name  AS from_table, kcu.column_name AS from_col,
       ccu.table_name AS to_table,   ccu.column_name AS to_col
FROM   information_schema.table_constraints      tc
JOIN   information_schema.key_column_usage       kcu USING (constraint_name)
JOIN   information_schema.referential_constraints rc  USING (constraint_name)
JOIN   information_schema.key_column_usage       ccu ON ccu.constraint_name = rc.unique_constraint_name
WHERE  tc.constraint_type = 'FOREIGN KEY';
```

### Epic D — Result & pagination semantics (document)

- **D1 — Result column metadata.** *As an app developer, results tell me the
  output column names/types so I can render a grid without a second call.*
  Priority: **Must** (already enriched; document it).
  - AC: Documents the `columns` array on `rows` results (names, and types if
    available), including for projections/joins/aggregates and `EXPLAIN`.
- **D2 — Pagination & cursors.** *As an app developer, I can page large results
  predictably.* Priority: **Should**.
  - AC: Documents keyset vs. `LIMIT/OFFSET` guidance and the cursor mechanism
    (`cursor.rs`, R4) for streaming large reads.

### Epic E — The reference document ("extract everything")

- **E1 — Application Builder's Guide.** *As an end user, I have one document that
  tells me how to connect and extract every piece of metadata and data, like the
  Postgres manual.* Priority: **Must**.
  - AC: A new `docs/engine_access_guide.md` (or a section set in `REST_API.md` +
    `design/`) that stitches Epics A–D into a task-oriented guide: connect →
    query → introspect (with the catalog recipes) → page → map types → handle
    errors. Linked from `documentation_index.md`.
  - AC: Includes a **"build a schema explorer in 30 lines"** recipe using only the
    documented surface, proving the contract is sufficient (the studio is the
    living proof).

---

## Milestone acceptance

- [ ] Epics A, B, C(C1–C3), D1, E1 complete (the **Must** set).
- [ ] The catalog queries in the worked example run against a live instance and
      return correct PK/FK data for a schema with real foreign keys.
- [ ] `unidb-studio` switches its schema visualizer from inferred edges to catalog
      foreign keys **with no engine change beyond this milestone** — the proof the
      surface is complete and app-owned.
- [ ] `documentation_index.md` links the new guide; `PROGRESS.md` carries the
      closeout entry (no metrics duplicated here).

## Open questions / landmines (surface first, per CONVENTIONS "de-risk")

1. **Catalog implementation strategy.** Real `information_schema` *views* over
   internal catalog structures vs. a bespoke `unidb_catalog` table-function set.
   Views are more Postgres-faithful; functions may be simpler given the current
   catalog representation. Decide before building C.
2. **DDL authority (C5).** Does the engine retain original `CREATE` text, or must
   DDL be reconstructed from metadata? Reconstruction is lossy for defaults/checks
   — state the chosen approach.
3. **Attach/wire vs. server parity.** The catalog must be reachable identically
   from embed, attach, and server so a tool works regardless of access path.
4. **Naming.** `information_schema` compatibility scope — how much to mirror vs. a
   lean native namespace. Aim: enough for standard tooling, no more.

## References

- `docs/REST_API.md` — current server surface & the "not PostgREST" design intent.
- `docs/backlog/rest_api_enrichment.md` (item 12) — the flat `/tables`
  introspection this milestone generalizes into a catalog.
- `docs/backlog/CONVENTIONS.md` — backlog naming/lifecycle this file follows.
- `unidb-embed`, `unidb-attach` — the non-server access paths.
- Consumer: the `unidb-studio` repo (schema visualizer, filters, DDL viewer) —
  the first application built on this surface.
