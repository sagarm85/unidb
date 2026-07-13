# Studio API readiness — browser search/filter surface + integration guide

**Type:** Improvement
**Status:** NOT STARTED

> A `unidb-studio`-driven bundle: close the two gaps that stop a **browser** from
> offering text search/filter over the generic `POST /sql` surface, and ship the
> concrete API-integration guide an app developer needs to build an ERP-style app
> (customer/product/SO/invoice/payment, PK/FK-linked) on unidb. Honors the
> Milestone-18 boundary — the fixes are SQL-surface (reachable via `/sql`), **no
> new app-shaped REST endpoints**.
>
> The two engine gaps are **G9 and G11 of item 19** (`19_sql_surface_gaps.md`);
> this item delivers them (completing them flips those item-19 entries done) plus
> the guide. Item 19 stays the umbrella SQL-gap tracker.

## Why now
Building the ERP app in the studio surfaced (2026-07-13) that the record browser
can offer **neither substring filters nor word search** over a browser:
- `col LIKE '%foo%'` → `SQL_UNSUPPORTED` (G9) — studio had to remove
  contains/starts/ends filters.
- Full-text is embed-only (`Engine::search_fulltext`); no `/sql` predicate (G11)
  — a browser can build a FULLTEXT index but never query it.
Everything else the ERP app needs (atomic multi-model txn, FK/ERD introspection,
realtime events + lag, vector `NEAR`, cursor paging) already ships — see the
integration guide deliverable (E3) for the full map.

## Scope

### E1 — G9: `LIKE` / `NOT LIKE` / `ILIKE` (MUST)
- Add a `Like { expr, pattern, negated, case_insensitive }` variant on **both**
  expression paths — the row-path `Expr` (`convert_expr`) AND the planner
  `QExpr` (`convert_qexpr`) — so it works with and without the planner and with
  literal or `$n`-bound patterns.
- SQL-standard semantics: `%` = any run, `_` = one char, `ESCAPE`; `ILIKE` =
  case-insensitive; NULL semantics (NULL LIKE x → NULL/false).
- Differential-test against SQLite (rusqlite dev-dep, Phase-4 precedent) across a
  pattern/op/NULL matrix. No storage/format impact.
- Optional (note, don't build): pure-prefix `'abc%'` → B-tree range optimization
  — follow-up.

### E2 — G11: full-text search over `/sql` (MUST)
- Add a full-text **predicate reachable in `WHERE`**, mirroring the existing
  `NEAR(col, […], k)` precedent so it needs **no new REST route** (works over
  `/sql` automatically, like `NEAR`). Recommended V1 syntax:
  `MATCH(column, 'query text')` — a boolean predicate usable in `WHERE`.
  (Postgres-style `col @@ to_tsquery(…)` may be added later as an alias — do NOT
  build tsquery machinery in V1.)
- Wire the parser + planner + executor arm to the existing
  `Engine::search_fulltext(xid, table, column, query)` (the index already exists
  via `CREATE INDEX … USING FULLTEXT`). Stay MVCC/RLS-correct
  (over-fetch-then-filter, same as `NEAR`).
- Test: build a FULLTEXT index, `SELECT … WHERE MATCH(body, 'invoice overdue')`
  returns the right rows over `/sql` (and the server route), embed + attach +
  server parity.

### E3 — Studio API integration guide (MUST, docs)
- New section (in `engine_access_guide.md` or a new `docs/studio_integration.md`
  linked from `documentation_index.md`): the **concrete request/response payloads**
  for building an app on unidb, walking the ERP example end-to-end:
  1. **Schema + FK** — `POST /sql CREATE TABLE … FOREIGN KEY … REFERENCES …`
  2. **ERD / introspection** — `POST /sql SELECT … FROM
     information_schema.referential_constraints` (+ tables/columns/
     key_column_usage, `unidb_catalog.indexes`)
  3. **The atomic multi-model transaction** — `POST /txn/begin` → N×
     `POST /sql` (`X-Txn-Id`) writing rows + a `VECTOR` column in one txn →
     `POST /txn/{id}/commit`; show that all rows + events + the new embedding are
     one all-or-nothing commit (the differentiator vs PG+pgvector+Debezium).
  4. **Realtime events** — `POST /tables/{t}/events` →
     `GET /events/subscribe?consumer=…&format=supabase|debezium` (SSE,
     `Last-Event-ID` resume) → `POST /events/ack`; lag via
     `POST /sql SELECT * FROM unidb_catalog.subscription_lag`.
  5. **Search** — vector `NEAR(embedding, […], k)` and full-text
     `MATCH(col, '…')` (E2), both over `/sql`.
  6. **Record browser** — `LIKE` filters (E1) + cursor paging
     (`POST /sql {"cursor":true}` → `GET /sql/cursor/{id}?limit=`).
  7. **Auth** — `Authorization: Bearer <JWT>` (verify-only).
- Each step: real request body + real response shape. This is the "which APIs +
  exact payloads for the studio" deliverable.

## Non-goals
- No app-shaped REST endpoints (`/customers`, `/invoices`) — everything is `/sql`
  + the existing generic routes (Milestone-18 boundary).
- Not the `@@`/`to_tsquery` Postgres FTS dialect (V1 = `MATCH(...)`).
- Not RLS per-tenant (item 24); not the studio UI itself (out of this repo).

## Acceptance
- [ ] `col LIKE/ILIKE/NOT LIKE pattern` works on both expr paths, literal + `$n`;
      differential-matches SQLite; item-19 G9 flipped done.
- [ ] `SELECT … WHERE MATCH(col, 'query')` returns correct rows over `/sql` and
      the server route (embed/attach/server parity); item-19 G11 flipped done.
- [ ] Integration guide walks the full ERP flow with real payloads; linked from
      `documentation_index.md`.
- [ ] Crash harness unchanged (pure query surface — no storage/format change);
      clippy/fmt/workspace green.

## Builds on
- Item 18 (catalog relations), item 29 (event formats + lag), item 12 (cursors,
  sessions), the existing `NEAR` + FULLTEXT index + `search_fulltext`.
- Disjoint from any storage/recovery lane; purely `src/sql/*` + docs.
