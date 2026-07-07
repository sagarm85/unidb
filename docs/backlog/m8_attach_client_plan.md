# M8 — Attach Client (Rust-only v1)

## Status as of 2026-07-07: NOT STARTED.

## Context
User-confirmed scope: Rust-only client for v1 (multi-language bindings
explicitly deferred — track as a new backlog entry when this ships, not
attempted now). Uses **blocking `reqwest`** (`reqwest::blocking::Client`)
for its sync-to-async bridge — no background thread/tokio runtime, since
that adds complexity a v1 client doesn't need per the user's own call.

The REST API (`src/server/router.rs`, `handlers.rs`, `dto.rs`,
`docs/REST_API.md`) already covers ~90% of embedded `Engine`'s public
surface 1:1. Confirmed gaps with NO REST route today: `vacuum_events`,
`set_rls_policy` (no JSON `Expr` serialization design exists), `flush`
(test-only). These are explicitly OUT of scope for v1 — do not add new
REST routes just to support them; document as known gaps.

## Key design decisions (final, not open for re-litigation)
- **Crate structure**: a new workspace member crate (e.g. `unidb-attach`),
  NOT a feature flag on the main `unidb` crate — keeps `reqwest` (a real,
  non-dev dependency for this crate) from ever touching the embedded
  engine's dependency graph, consistent with the existing "engine stays
  sync, server deps are feature-gated" discipline already established
  for the `server` feature. This repo currently has no top-level
  `[workspace]` in `Cargo.toml` — you'll need to add one (with `unidb`
  itself becoming a workspace member) as the first step.
- **API shape**: attach client's public methods are **one-shot calls**
  matching what REST actually offers (`execute_sql`, `insert`, etc. each
  do their own internal begin->execute->commit, exactly like the REST
  routes already do server-side) — NOT a literal mirror of embedded
  `Engine`'s explicit separate `begin`/`commit` calls, since multi-request
  transaction sessions don't exist over HTTP (confirmed: `POST /txn/begin`
  exists only for introspection/debugging, per `docs/REST_API.md`).
  Document this explicitly as a deliberate API-shape difference from
  embedded `Engine`, not an oversight — multi-statement atomicity is still
  available via one `;`-separated `execute_sql` call, same as REST.
- **Error type**: a new `AttachError` enum (not a reuse of `unidb::error::
  DbError`, whose variants are storage/engine-internal — e.g.
  `PageNotFound`, `ChecksumMismatch` — and don't cleanly map back from an
  HTTP response). Must capture at least the client-facing error codes
  already documented in `docs/REST_API.md`'s error table (`TABLE_NOT_FOUND`,
  `WRITE_CONFLICT`, etc.) plus network-level failures (connection refused,
  timeout, JSON deserialization failure) that have no `DbError` equivalent.
- **Auth**: accepts a pre-signed JWT string at construction (matching
  `scripts/gen_jwt.sh`'s output) and attaches it as `Authorization: Bearer`
  on every request — no new server-side auth capability needed.

## Checkpoints
- **M8.a**: Add `[workspace]` to the root `Cargo.toml`, new `unidb-attach`
  crate scaffold (workspace member), `AttachClient::new(base_url,
  jwt_token)`, `AttachError`, and the simplest routes (`execute_sql`,
  `execute_cypher`, raw row CRUD).
- **M8.b**: Graph routes (`create_edge`/`delete_edge`/`edges_from`),
  indexing routes (`set_column_index`/`index_status`), events
  (`enable_events`/`ack_events` — `poll_events` stays SSE-only, no
  polling JSON route exists to wrap), `checkpoint`.
- **M8.c**: Tests (a real `unidb-server` spun up in-test, `AttachClient`
  round-trips against it — mirror `tests/server_*.rs`'s existing
  `TestServer` pattern), benchmark (attach-client overhead vs. direct
  embedded `Engine` calls and vs. raw `reqwest` calls to the same routes,
  extending `benches/server.rs`'s pattern), `PROGRESS.md`/`MEMORY.md`
  closeout.

## Known limitations to document
- No multi-request transaction sessions (matches REST's own limitation).
- `vacuum_events`, `set_rls_policy`, `flush` not exposed — no REST route
  exists for them.
- Blocking I/O — each call blocks the calling thread; not suited to
  highly concurrent same-process usage without the caller managing their
  own thread pool.

## Backlog (explicitly deferred, not part of M8 v1)
- Multi-language client bindings (Python/Node) — same REST contract,
  different language, whenever there's a concrete need.

## Note on parallel work
M6 (BTree) and M7 (CSR) are being worked on concurrently in sibling
worktrees. M8 is REST-surface-only work and shouldn't touch the same
engine-internal files as M6/M7, so conflict risk on merge is low.
