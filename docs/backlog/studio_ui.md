# Studio UI — a separate project that demonstrates the engine over REST

## Status as of 2026-07-10: NOT STARTED (backlog).

A clickable demonstration of unidb's capabilities — SQL editor, table browser,
query timing, CSV load — built as a **separate project** (`unidb-studio`), not
in this repo. The engine repo's only responsibility is the **contract**
(`docs/REST_API.md`) plus one small enabler endpoint. Everything front-end lives
outside.

## Architecture decision (locked here): the UI is a separate project

The studio is its own repo/deployable that talks to a running `unidb-server`
**only** through the documented REST API + a JWT. It is not bundled into, or
served by, the engine. Rationale:

- **Enforces the contract instead of trusting it** — a separate app can only use
  `docs/REST_API.md`; it cannot reach into internals or share Rust types. So it
  is unfakeable proof the public API is complete and usable.
- **Right separation** — the engine is careful systems Rust (ACID, on-disk
  format); a UI is fast-moving browser code (npm/bundlers). Keeping them apart
  respects §1's "no cloud control plane" non-goal (a studio is adjacent to it).
- **Ecosystem-standard** — `supabase/studio` and `supabase-js` are separate from
  Postgres; pgAdmin is separate from Postgres. `unidb-attach` already set this
  precedent (a separate crate over REST).
- **Independent deploy/scale/velocity** — UI = stateless static assets (CDN);
  engine = stateful singleton container. Different lifecycles.

**The seam is `docs/REST_API.md` + JWT.** CORS is already wired
(`CorsLayer::permissive()` in `server/router.rs`), so a browser app on another
origin can already call the API.

## Engine-side work (the ONLY changes in THIS repo)

### S1 — `GET /tables` introspection endpoint
- `catalog.tables()` already yields every `TableDef`; there is no REST route for
  it. Add `GET /tables` → `[{ name, columns:[{name, type, nullable, index?}], row_count? }]`
  (row_count optional/estimated — a full count may be expensive; a cheap
  estimate or omit in v1). Auth-gated like the other routes.
- Files: `server/router.rs`, `server/handlers.rs`, `server/dto.rs`;
  `tests/server_*.rs` round-trip; **`docs/REST_API.md`** entry.

### S2 (optional, later — not required for the demo)
- `POST /rows/batch` (fast CSV — one txn, N inserts; already scoped in
  `rest_api_enrichment.md` R4).
- Execution timing in the `/sql` response body (the executor already measures it
  — `query_exec.rs` computes elapsed; surface it so the UI need not rely on
  `EXPLAIN ANALYZE`).

Everything else the studio needs is already served by `POST /sql`.

## The separate `unidb-studio` project (built against the contract)

Static SPA (vanilla JS, or Svelte/React + Vite). Panels:

1. **SQL editor (CRUD)** — textarea → `POST /sql` → render rows/affected-count.
2. **Database + tables list** — `GET /tables`. Note: unidb is **single-database**
   (one data dir) — present as *database → tables*, not Postgres-style multi-DB.
3. **Table records + paging** — click a table → `SELECT * FROM t ORDER BY <pk>
   LIMIT n` for the first page, then **keyset paging** (`WHERE pk > $last ORDER BY
   pk LIMIT n`) for "next" (offset paging via `LIMIT/OFFSET` is the simpler
   fallback). All via `/sql`.
4. **Join/filter editor + timing** — arbitrary join/filter SQL via `/sql`; show
   **fetch time**, labeled honestly: *server execution time* (from `EXPLAIN
   ANALYZE` or the S2 timing field) and/or *client round-trip* — they answer
   different questions; don't conflate.
5. **CSV upload + timing** — parse CSV in the browser → batched `INSERT`s in one
   transaction (fast now that commit is one fsync) → show wall-clock. Caveat:
   **no native `COPY`/bulk path**, so it is per-row INSERT under the hood — fine
   for demo-sized files; `POST /rows/batch` (S2) speeds it up later.

### Auth (design honestly)
- The browser must **never** hold the HS256 `UNIDB_JWT_SECRET` (symmetric —
  anyone with it mints tokens).
- **Demo:** paste a pre-minted dev JWT (`scripts/gen_jwt.sh`).
- **Real:** a thin backend-for-frontend holds the secret, does login, mints
  short-lived tokens (the GoTrue role in the Supabase model). This BFF is part of
  the studio project, not the engine.

## Verification / done

- **This repo:** `GET /tables` has a server test; `docs/REST_API.md` documents it;
  standard gates green (build/test/clippy/fmt/sync-invariant). No other engine
  change.
- **`unidb-studio` repo (separate):** its own build/tests; depends only on the
  REST contract + a JWT. Out of scope for this repo's CI.

## Non-goals / honesty notes

- The UI code does **not** live in the unidb repo.
- No multi-database (single data dir = one database) — don't imply otherwise.
- No native CSV `COPY`; demo-sized files only until `POST /rows/batch` lands.
- Consuming the API from a separate app makes REST a **published contract** —
  route/DTO changes now need versioning discipline. That is the (worthwhile)
  cost of the separation.
