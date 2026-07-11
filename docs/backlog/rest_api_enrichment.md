# REST API enrichment — transaction sessions & full-surface coverage

**Type:** Improvement
**Status:** ✅ SHIPPED (2026-07-11) — all four checkpoints (R1–R4), branch
`claude/rest-api-enrichment-vly934`. Metrics + full detail in `PROGRESS.md`'s
"REST API enrichment (item 12)" entry; route contracts in `docs/REST_API.md`.
Every verification gate below passed (server suite grew by 24 integration
tests across `tests/server_txn.rs` + `tests/server_enrich.rs`; crash harness
untouched at 29 — no storage-path change). Deviations from this spec, decided
during implementation and documented in `REST_API.md`: DDL (catalog + auth) is
**rejected** inside a session (engine DDL rollback is request-scoped, P2.c —
rejecting beats silently-unrollbackable); a failed *mutating* session
statement auto-aborts the session (Postgres-without-savepoints), while failed
pure reads leave it open; large results shipped as a **cursor**
(`POST /sql {"cursor": true}` → `GET /sql/cursor/{id}?limit=`), not NDJSON —
with the documented honest caveat that rows stay buffered (decoded)
server-side because the executor is sync; the attach client stays one-shot
(follow-up unchanged).

## Original status as of 2026-07-09: NOT STARTED. Sequenced AFTER Phase 6 merges.

Phase 6 (ops/HA) is actively rewriting `server/*` (P6.e users/roles/GRANT, P6.f
security, P6.g observability). This work touches the same files (router, auth
middleware, DTOs, error map), so it must land on a **clean post-Phase-6 base** to
avoid conflicts. Do not start it on a branch parallel to the Phase 6 PR.

## Context — what's already covered (do NOT rebuild)

A lot of the engine's power is already reachable; the REST layer is thinner than
it looks because SQL is a single pass-through endpoint:

- **Joins, aggregation, GROUP BY/HAVING, ORDER BY, DISTINCT, subqueries, CTEs,
  EXPLAIN** — all reachable via **`POST /sql`** already (they're SQL; the Phase 4
  executor does the work). Prepared statements work via `/sql`'s `params`.
- **Concurrent reads/writes** — Phase 5 (P5.e-3) already made the server share
  `Arc<Engine>` via `spawn_blocking`, so concurrent HTTP requests run through the
  worker pool **transparently**. No endpoint work needed.
- **Failures / error recovery** — recovery is internal (ARIES on open). Failures
  surface via the documented error→HTTP map (`409` write-conflict/deadlock/
  serialization, `503` durability-failure, `4xx` client errors). Already done.

So this spec is **not** about joins/aggregation/concurrency — those ship. It is
about the genuine gaps below, chiefly **multi-statement transactions over HTTP**.

## Scope

- **IN:** transaction sessions (multi-statement `BEGIN…COMMIT` over HTTP),
  per-transaction isolation selection, the deferred M8 routes
  (`vacuum_events`/`set_rls_policy`/`flush`), batch insert, large-result
  pagination.
- **OUT:** a new wire protocol (stay REST/JSON); Postgres wire-protocol
  compatibility (parked); health/readiness + richer observability endpoints
  (**owned by Phase 6 P6.g — coordinate, don't duplicate**).

## Checkpoints

### R1 — Transaction sessions (the headline gap)

Today every mutating request is its own transaction; `POST /txn/begin` is
introspection-only ("Known limitations" in `REST_API.md`). Expose a real,
client-held transaction handle.

**Wire shape**
- `POST /txn/begin` `{ "isolation": "read_committed" | "repeatable_read" | "serializable" }`
  → `201 { "txn_id": <xid>, "isolation": "...", "expires_at": "<ts>" }`. The
  server keeps the txn open (the `Engine` already tracks active txns by `xid`).
- Subsequent statement requests (`/sql`, `/rows`, `/cypher`, `/edges`, …) carry
  the session via header **`X-Txn-Id: <xid>`**. When present, the server runs the
  op under that `xid` and does **not** auto-commit.
- `POST /txn/{txn_id}/commit` → commit; `POST /txn/{txn_id}/rollback` → abort.
- With no `X-Txn-Id`, behavior is unchanged (one-shot auto-commit txn).

**Hard design points (must be handled, not hand-waved):**
1. **In-session serialization.** A single txn's state (undo log, snapshot,
   held locks) is *not* safe for two concurrent requests on one `xid`. The
   server must serialize requests bearing the same `X-Txn-Id` (per-session
   `Mutex`/actor), even though different sessions run concurrently. A second
   concurrent request on a busy session returns `409 TXN_BUSY`.
2. **Idle-session timeout + reaper.** An abandoned open txn holds row locks
   **and pins the MVCC vacuum horizon** (→ bloat). Every session gets an idle
   deadline (reuse P5.f's timeout machinery); a background reaper auto-aborts
   expired sessions and frees them. Non-negotiable — a dropped client must not
   leak a horizon-pinning txn.
3. **Principal binding.** A session is bound to the JWT principal that created
   it; a different principal presenting the `txn_id` gets `403`. Prevents
   session hijacking.
4. **Ephemerality.** Open sessions do not survive a server restart (recovery
   aborts in-flight txns). Document that `txn_id`s are ephemeral; a stale one
   returns `404 TXN_NOT_FOUND`.

**Tests:** multi-statement commit is atomic (all-or-nothing on rollback);
`repeatable_read` session sees a stable snapshot across requests; idle session is
auto-aborted and its locks/horizon released; concurrent request on a busy session
→ `409`; cross-principal access → `403`; stale `txn_id` → `404`.

### R2 — Isolation selection for one-shot statements
- Optional `isolation` field on `POST /sql` (and other one-shot mutating routes)
  for a single-statement txn at a chosen level, without opening a session.
- Test: a one-shot `serializable` write-skew attempt is rejected with `409
  SERIALIZATION_FAILURE`.

### R3 — Deferred routes (from the M8 backlog)
- **`POST /events/vacuum`** (or `DELETE /events?up_to_seq=`) → `Engine::
  vacuum_events`; returns reclaimed count. Honor the slow-consumer durability
  contract (M4).
- **`PUT /tables/{table}/rls`** → `set_rls_policy`. **Blocker to resolve first:**
  there is no `Expr`↔JSON design (M8 note). Options: accept a **SQL predicate
  string** the server parses into an `Expr`, or a small JSON policy DSL. Pick and
  document one; the string-predicate path reuses the existing parser and is the
  low-risk choice.
- **`POST /admin/flush`** → `Engine::flush` (today test-only). Gate behind an
  admin role (Phase 6 P6.e) once roles exist.

### R4 — Batch & pagination
- **`POST /rows/batch`** `{ "rows": [<bytes>, …] }` → one txn, N inserts, returns
  the `RowId`s (bounded batch size).
- **Large SQL results:** stream big result sets as chunked **NDJSON** (or a
  cursor: `POST /sql` returns a `cursor_id`, `GET /sql/cursor/{id}?limit=` pages
  it) so a multi-GB scan doesn't buffer the whole JSON array in memory.
- Tests: batch insert is atomic; a cursor paginates a >buffer result correctly
  and expires on idle.

## Locked decisions / constraints

- **Engine stays sync** (`CLAUDE.md` §4): all of this is server-side (`server/*`,
  behind the `server` feature); the sync-invariant check must stay clean.
- **Auth unchanged in spirit:** verify-only JWT (M5.c) + principal binding for
  sessions. Full user management is Phase 6 P6.e, not here.
- No §3 storage/txn decision is touched — this is a surface/protocol layer over
  the existing `Engine::begin/execute/commit/abort` API.

## Verification gates (done =)

- `cargo test -p unidb --features server` green, incl. new integration tests
  (reuse the `TestServer` harness) for every R1 design point above.
- `cargo clippy --workspace --all-targets -- -D warnings` + `cargo fmt` clean.
- **Sync invariant** (`cargo tree -p unidb --no-default-features --edges normal`
  free of tokio/reqwest/axum) still holds — none of this leaks into the engine.
- `docs/REST_API.md` updated (new routes, the `X-Txn-Id` header, new error codes
  `TXN_BUSY`/`TXN_NOT_FOUND`, and the transaction-session section); README + the
  Rust attach client (M8) note whether/how they support sessions.

## Known limitations / deferred

- No Postgres wire-protocol compatibility (parked).
- The Rust attach client (M8) stays one-shot by default; session support is an
  optional follow-up on top of R1.
- Health/readiness + richer observability endpoints are **Phase 6 P6.g** — this
  spec defers to it.
