# Engine access & introspection contract — "build your app on the engine"

**Type:** Milestone
**Status:** SHIPPED (2026-07-13) — see `PROGRESS.md` "Engine access &
introspection contract (Milestone 18)". The Must set (A1, B1, B2, B3, C1, C2,
C3, D1, E1) plus the Should items that fell out cheaply (A2, B4, C4, C5, D2)
landed; the studio-switchover acceptance box below stays **unticked** — it
closes from the `unidb-studio` repo, not here.

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

-- foreign keys (drives the ERD edges + FK badges).
-- Explicit ON form (see design-note landmine 1a: JOIN ... USING / NATURAL are
-- not in the SQL surface yet, so the recipe uses the equivalent ON form; the
-- `ccu.ordinal_position = kcu.position_in_unique_constraint` conjunct aligns
-- each FK column with its referenced column for COMPOSITE keys).
SELECT tc.table_name  AS from_table, kcu.column_name AS from_col,
       ccu.table_name AS to_table,   ccu.column_name AS to_col
FROM   information_schema.table_constraints      tc
JOIN   information_schema.key_column_usage       kcu
       ON kcu.constraint_name = tc.constraint_name
JOIN   information_schema.referential_constraints rc
       ON rc.constraint_name  = tc.constraint_name
JOIN   information_schema.key_column_usage       ccu
       ON ccu.constraint_name = rc.unique_constraint_name
      AND ccu.ordinal_position = kcu.position_in_unique_constraint
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

- [x] Epics A, B, C(C1–C3), D1, E1 complete (the **Must** set). Should items
      A2/B4/C4/C5/D2 also folded in (documented in the guide; C4 =
      `unidb_catalog.indexes`; C5 = documented reconstruction rules).
- [x] The catalog queries in the worked example run against a live instance and
      return correct PK/FK data for a schema with real foreign keys —
      `tests/information_schema.rs::worked_example_fk_join_pairs_composite_columns`
      proves it on **composite** PK/FK, columns correctly paired.
- [ ] `unidb-studio` switches its schema visualizer from inferred edges to catalog
      foreign keys **with no engine change beyond this milestone** — the proof the
      surface is complete and app-owned. **(Unticked here by design — this box
      closes from the `unidb-studio` repo. The engine surface it needs is
      complete and proven by the differential + parity tests.)**
- [x] `documentation_index.md` links the new guide (`docs/engine_access_guide.md`);
      `PROGRESS.md` carries the closeout entry (no metrics duplicated here).

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

## Design note & landmine decisions (2026-07-13)

Recorded before any Epic-C code, per CONVENTIONS "de-risk first" and the plan's
"decide the landmines first". These bind the implementation.

### 0 — FK DDL already parses & persists (unlisted prerequisite → option (a))

Verified in the code, not assumed: `FOREIGN KEY (…) REFERENCES …` and column
`REFERENCES` **already parse and persist today** (M11). `src/sql/parser.rs`
maps `ast::ColumnOption::ForeignKey` → `ColumnConstraints.references`
(`ForeignKeyRef`) and `ast::TableConstraint::ForeignKey` →
`TableConstraints.foreign_keys` (`ForeignKey { columns, ref_table,
ref_columns }`), both in `src/catalog.rs`. `PRIMARY KEY` (column + table-level),
`UNIQUE` (column + table-level), and `CHECK` likewise persist. So the milestone
lands squarely on **option (a): FK is stored + introspectable metadata; this
milestone does NOT add row-level enforcement** — that stays as M11 documented
(referenced-*table*-existence only; referenced-*row* existence + `ON
DELETE/UPDATE` actions remain a filed follow-up). Epic C is therefore *pure
read-side projection over metadata that already exists on disk* — no catalog
schema change, no `FORMAT_VERSION` bump, no new persisted field. (The catalog
blob is JSON with `#[serde(default)]` throughout; nothing here even touches it.)
This is called out again in the guide's honest-limitations list.

### 1 — Catalog strategy: synthesized virtual relations resolved at plan time

**Decision:** the catalog relations are **virtual/synthesized relations** —
*not* on-disk tables, *not* a table-function syntax. When a `FROM` name is one
of the reserved introspection names (`information_schema.*` / `unidb_catalog.*`)
the planner supplies a fixed synthetic schema and the runner materializes the
rows from the live in-memory `Catalog` at scan time. Consequences, all
desirable: **always current** (computed from the catalog on every query), **no
storage** (no heap, no pages, no vacuum/MVCC interaction, no crash-harness
surface — the count stays 31), and **reachable from every access path for free**
because embed / attach / server all funnel through the one
`Engine::execute_sql → executor::execute → exec_query` path (landmine 3 resolved
by construction, not by parity glue).

Routing (confirmed against the executor): a single-table `SELECT … FROM
information_schema.columns` would otherwise become a row-at-a-time
`LogicalPlan::Select`; we force any SELECT whose base relation is an
introspection relation onto the `LogicalPlan::Query` path in the parser so
**one** virtual-scan implementation serves single-table *and* multi-way-JOIN
queries. The cost-based optimizer bails to the rule-based `plan_from` for any
relation without `ANALYZE` stats (virtual relations never have stats), so
`plan_from` (schema) + `Runner::scan` (rows) are the only two interception
points, plus a guard on the `COUNT(*)` parallel fast path.

- **1a — `JOIN … USING` / `NATURAL JOIN` are not in the SQL surface yet.** The
  spec's original worked example used `USING (constraint_name)`; the parser
  supports only `ON <expr>`. This is a *syntax* gap, **not** a
  virtual-relation-join gap — the relations join fine over `ON`. Per the plan's
  honesty bar (don't fake a JOIN with a bespoke endpoint) the worked example is
  rewritten to the **equivalent explicit-`ON` form** above (adding the
  `ordinal_position = position_in_unique_constraint` conjunct that composite-key
  alignment needs), and `USING`/`NATURAL` are listed under B1 "not supported
  yet". No AC weakened: the ERD queries run against a live instance and return
  correct composite PK/FK rows (differential test).

### 2 — C5 object DDL: reconstruct from metadata, do not store CREATE text

**Decision:** unidb does **not** retain original `CREATE …` text (verified: the
catalog stores structured `TableDef`, never the source string), and adding a
`object_ddl(<name>)` **table-function** would need parser + executor
table-function support that does not exist — out of proportion to a *Should*
story. So C5 is satisfied by its second AC branch: the guide **documents the DDL
reconstruction rules** from C1–C4 (column list + types + nullability/defaults +
PK/UNIQUE/CHECK/FK constraints + indexes), and states honestly that the
reconstruction is canonical-but-not-byte-identical (whitespace, and any DEFAULT
expression is re-rendered from the parsed literal). A studio "View DDL" action
builds the text client-side from the catalog relations — exactly the
app-owns-its-surface thesis.

### 3 — Attach/server parity is structural

All three access paths reach the same executor (embed calls `Engine::
execute_sql`; the server's `POST /sql` calls it via `EngineHandle`; `unidb-
attach` POSTs to that same route). The catalog relations live below all of them,
so a parity test only has to prove the *same* query returns the *same* rows over
embed, `unidb-attach`, and the server `/sql` route — which it does.

### 4 — `information_schema` scope: lean, standard-tooling-shaped

Mirror only what the studio/ERD needs and what standard tooling expects:
`tables`, `columns`, `table_constraints`, `key_column_usage`,
`referential_constraints` under `information_schema`; `indexes` under the native
`unidb_catalog` namespace (Postgres exposes indexes via `pg_catalog`/`pg_indexes`,
not `information_schema`, so a native name is more honest than a fake
`information_schema.statistics`). `table_schema`/`table_catalog` are reported as
the constant `'public'` / the database name — unidb has no schema namespacing,
and saying so plainly beats inventing one. Columns present on each relation are
the standard-named subset the worked example and common tools read; unused
standard columns are omitted rather than filled with guesses.

## References

- `docs/REST_API.md` — current server surface & the "not PostgREST" design intent.
- `docs/backlog/rest_api_enrichment.md` (item 12) — the flat `/tables`
  introspection this milestone generalizes into a catalog.
- `docs/backlog/CONVENTIONS.md` — backlog naming/lifecycle this file follows.
- `unidb-embed`, `unidb-attach` — the non-server access paths.
- Consumer: the `unidb-studio` repo (schema visualizer, filters, DDL viewer) —
  the first application built on this surface.
