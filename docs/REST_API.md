# unidb REST API Reference

Covers the optional `unidb-server` binary (M5, gated behind the `server`
Cargo feature). Source of truth for this document: `src/server/router.rs`,
`handlers.rs`, `dto.rs`, `auth.rs`, `sse.rs`, `error.rs`,
`txn_session.rs` (transaction sessions, R1), `cursor.rs` (result cursors,
R4).

This is a thin HTTP wrapper over the embedded `Engine`. By default every
mutating route runs exactly one `begin -> execute -> commit-or-abort`
cycle; since Phase 5 (P5.e-3) requests execute **concurrently** over one
shared `Arc<Engine>` via `spawn_blocking` (`src/server/engine_handle.rs`;
an earlier version of this document described the retired M5
single-writer-thread design). Requests may instead join a client-held
**transaction session** via the `X-Txn-Id` header — see
[Transaction sessions](#transaction-sessions). It is **not** a
resource-oriented, auto-generated API in the PostgREST sense — `/sql` and
`/cypher` accept raw query text in the request body.

---

## Conventions

- **Base URL**: `http://<UNIDB_BIND_ADDR>` (default `http://127.0.0.1:8080`).
- **Auth**: every route below except `GET /metrics` requires
  `Authorization: Bearer <jwt>`. See [Authentication](#authentication).
- **Content type**: JSON routes send/receive `application/json`. `POST /rows`,
  `GET /rows/{page_id}/{slot}`, and `PUT /rows/{page_id}/{slot}` use raw
  bytes (`application/octet-stream` — the body is opaque row payload, not
  parsed as JSON by the server).
- **Errors**: every non-2xx JSON response has the shape:

  ```json
  { "error": "human-readable message", "code": "MACHINE_READABLE_CODE" }
  ```

  See [Error codes](#error-codes) for the full status/code table.
- **Transactions**: with no `X-Txn-Id` header, every route is a single,
  complete, self-contained transaction (multi-statement atomicity in one
  request via a `;`-separated `/sql` body). With an `X-Txn-Id` header, the
  request runs inside an open [transaction session](#transaction-sessions)
  and does **not** auto-commit. (Historical note: before the REST-enrichment
  work, `POST /txn/begin` was introspection-only with no way to commit over
  a later request — that limitation is gone.)

---

## Authentication

Verify-only, stateless JWT (HS256). The server validates a bearer token
signed with a shared secret (`UNIDB_JWT_SECRET`) — there is no login
endpoint, no user database, and no session state. Anything that issues
tokens (an external auth service, a secret shared out-of-band) is outside
this project's scope.

```
Authorization: Bearer <jwt signed with UNIDB_JWT_SECRET, HS256>
```

For local testing, generate a token with `scripts/gen_jwt.sh` (pure bash +
`openssl`, no Python/PyJWT install required):
```bash
TOKEN=$(UNIDB_JWT_SECRET=dev-secret ./scripts/gen_jwt.sh)
```

A validly-signed, unexpired token is required on every data-plane route. The
JWT `sub` claim is the acting username; a token with no `sub` is the implicit
superuser. With no roles registered the server is in **open mode** (all users
have full access — backward compatible). Once roles and grants are registered
(see [Authorization — roles, grants, RLS](#authorization--roles-grants-and-rls-item-24)),
a missing privilege returns `403 PERMISSION_DENIED`. Missing,
malformed, wrong-signature, or expired tokens all return:

```
HTTP 401 Unauthorized
{ "error": "invalid token: ExpiredSignature", "code": "UNAUTHORIZED" }
```

`GET /metrics` is the one route that never requires a token (Prometheus
scrapers don't carry app bearer tokens — firewall it at the network layer
in production instead).

---

## Transaction sessions

A **transaction session** is a real, client-held engine transaction spanning
multiple HTTP requests (REST enrichment R1).

### `POST /txn/begin`

**Payload** (optional; empty body = `read_committed`):
```json
{ "isolation": "read_committed" | "repeatable_read" | "serializable" }
```

**Response** `201 Created`:
```json
{
  "txn_id": 42,
  "xid": 42,
  "isolation": "read_committed",
  "idle_timeout_secs": 60,
  "expires_at": "2026-07-11 12:34:56"
}
```
`xid` is a compatibility alias for `txn_id` (the field name of the old
introspection-only route). `expires_at` is the **sliding** idle deadline:
every completed request on the session pushes it out by
`idle_timeout_secs` again.

### Statements inside a session

`POST /sql`, `POST /cypher`, `POST /rows`, `POST /rows/batch`,
`GET/PUT/DELETE /rows/{page_id}/{slot}`, `POST /edges`,
`DELETE /edges/{page_id}/{slot}`, and `GET /edges/from/{from_id}` accept:

```
X-Txn-Id: <txn_id>
```

The operation then runs under that transaction and does **not**
auto-commit. The session sees its own uncommitted writes; a
`repeatable_read`/`serializable` session keeps one stable snapshot across
all its requests.

### `POST /txn/{txn_id}/commit` · `POST /txn/{txn_id}/rollback`

Finish the session. `200 OK` with `{"txn_id": 42, "state": "committed"}`
(or `"rolled_back"`). Either way the `txn_id` is gone afterwards — a
`SERIALIZATION_FAILURE` on commit (SSI, P1.d) reports `409` on an
already-rolled-back, fully cleaned-up transaction; the client just
re-begins and retries.

### Session rules (the contract)

- **One statement at a time.** A session's transaction state is not safe
  for concurrent requests; a second request while one is executing gets
  `409 TXN_BUSY` (other sessions and one-shot requests are unaffected —
  they run concurrently).
- **Idle sessions are reaped.** An abandoned open transaction holds row
  locks and pins the MVCC vacuum horizon, so a background reaper
  auto-aborts any session idle longer than `UNIDB_TXN_IDLE_TIMEOUT_SECS`
  (default 60). A reaped/finished/unknown `txn_id` returns
  `404 TXN_NOT_FOUND`.
- **Principal-bound.** The session belongs to the JWT `sub` that created
  it; another principal presenting the id gets `403 TXN_FORBIDDEN`.
- **Ephemeral.** Session ids do not survive a server restart (recovery
  aborts in-flight transactions).
- **No DDL.** Catalog DDL (`CREATE/ALTER/DROP/TRUNCATE/ANALYZE`) and auth
  DDL are rejected inside a session with `400 DDL_IN_SESSION` — the
  engine's DDL rollback is request-scoped (P2.c), not transaction-scoped.
  Run DDL as one-shot requests.
- **A failed mutating statement aborts the session** (it may have left
  partial effects): the transaction is rolled back and the `txn_id`
  destroyed — Postgres-without-savepoints semantics. Failed *pure reads*
  (`GET /rows/…`, `GET /edges/from/…`) leave the session open; requests
  rejected before execution (busy, DDL, authorization) also leave it open.
- **Isolation is fixed at begin**; an `isolation` field on a session
  statement returns `400 ISOLATION_IN_SESSION`.
- An open session blocks the quiescence-gated auto-checkpoint (P1.e) like
  any open transaction — another reason the idle reaper is non-negotiable.

---

## Routes

### `POST /sql`

Execute one or more `;`-separated SQL statements atomically under a single
transaction. If any statement fails, the entire request is rolled back —
including earlier statements in the same body.

> **Correction (P2.c, 2026-07-08):** an earlier version of this doc said
> catalog DDL is "never rolled back." That is no longer true — P2.c added
> **request-level DDL rollback**: DDL (`CREATE`/`ALTER`/`DROP`/`TRUNCATE`)
> persisted by earlier statements of a failed multi-statement request is now
> restored. (Full crash-safe, user-transaction-scoped catalog undo through
> recovery is still a follow-up; see `PROGRESS.md`'s P2.c entry.)

**Payload**:
```json
{ "sql": "CREATE TABLE t (id INT, name TEXT); INSERT INTO t (id, name) VALUES (1, 'alice')" }
```

**Bind parameters (P2.e)** — the injection-safe form. Supply `$n` placeholders
in `sql` and a positional `params` array; each value is bound as **data**,
never re-parsed as SQL:
```json
{ "sql": "INSERT INTO t (id, name) VALUES ($1, $2)", "params": [1, "alice"] }
```
A JSON string binds as text (later coerced to the column's type — UUID,
TIMESTAMP, etc.), a number as int/float, a numeric array as a vector. Omitting
`params` (or an empty array) runs the SQL as-is.

**One-shot isolation (R2)** — optional `isolation` field
(`"read_committed"` | `"repeatable_read"` | `"serializable"`) runs the
request as a single transaction at that level without opening a session
(e.g. a lone `serializable` statement participates in SSI conflict
detection and can be refused with `409 SERIALIZATION_FAILURE`). An
explicit level takes the transactional path (skipping the concurrent-read
fast path) so the chosen level actually governs the statement. Rejected
inside a session (`400 ISOLATION_IN_SESSION`).

**Cursor mode (R4)** — `"cursor": true` requires the request to be exactly
one rows-producing statement (SELECT/query/EXPLAIN — validated **before**
execution, `400 CURSOR_NOT_ROWS` otherwise), buffers the result
server-side, and responds with a cursor instead of the rows:
```json
{ "cursor_id": 7, "columns": ["id", "body"], "row_count": 120000 }
```
Page it with [`GET /sql/cursor/{id}`](#get-sqlcursorcursor_id--delete-sqlcursorcursor_id).

**Response** `200 OK` — one result object per statement, in order:
```json
{
  "results": [
    { "type": "created_table" },
    { "type": "inserted", "count": 1 }
  ]
}
```

Other `ExecResult` shapes:
```json
{ "type": "created_index" }
{ "type": "updated", "count": 3 }
{ "type": "deleted", "count": 1 }
{ "type": "altered_table" }
{ "type": "dropped_table" }
{ "type": "truncated", "count": 5 }
{
  "type": "rows",
  "columns": ["id", "name", "profile"],
  "rows": [
    [1, "alice", { "status": "active" }]
  ]
}
```
`columns` is the output column names in order (for `SELECT *`, the table's
non-dropped columns; for an explicit projection, the projected names; for
aggregates/joins, the resolved output names; `EXPLAIN` returns a single
`"QUERY PLAN"` column). `rows` is an array of arrays (one array per row);
each row's values align positionally with `columns`, so a client can zip them
into named fields.
A `JSON` column re-parses into a real nested JSON value on the wire — never
a JSON-encoded string (see `dto.rs`'s module doc for why). A `DECIMAL` column
serializes as a **decimal string** (e.g. `"9.90"`) and a `TIMESTAMP` as a UTC
string (`"2024-01-01 12:00:00"`) so no precision is lost through JSON's `f64`
numbers.

**Phase 4 query power (P4.a–P4.e).** `POST /sql` gained joins, aggregates /
`GROUP BY` / `HAVING`, `ORDER BY` / `DISTINCT` / `LIMIT` / `OFFSET`, subqueries
and `WITH` CTEs, `ANALYZE <table>` (gather optimizer statistics), and
`EXPLAIN [ANALYZE] <query>` — all through this same route with **no new routes
or error codes**. A `SELECT`/join/aggregate query returns the `rows` shape
above; `ANALYZE` returns an empty `rows` result; `EXPLAIN [ANALYZE]` returns
the plan as a `rows` result with one single-string column per plan line (and,
under `ANALYZE`, trailing `actual_rows=…` / `execution_time_ms=…` lines).

**Response on failure** — e.g. a later statement references a nonexistent
table, rolling back the whole request:
```
HTTP 404
{ "error": "table not found: nonexistent_table", "code": "TABLE_NOT_FOUND" }
```

---

### `POST /batch-sql`

Execute up to **256 independent one-shot SQL statements** in a single HTTP
round-trip, amortising the per-request fsync and network overhead (~10 ms/call
on typical setups). Each statement is auto-committed independently — there is
**no shared transaction** across the batch.

**Payload**:
```json
{
  "statements": [
    "SELECT COUNT(*) FROM t",
    "SELECT * FROM t WHERE id = 1",
    "INSERT INTO t (id, name) VALUES (3, 'carol')"
  ],
  "stop_on_error": false
}
```

`stop_on_error` (default `false`):
- `false` — all statements are attempted regardless of earlier failures; failed
  slots get a `null` result and an error string.
- `true` — stop at the first error; remaining slots get `null` result +
  `"skipped"` error string.

**Response** `200 OK` — always `200`; per-statement failures appear inside the
payload, not as HTTP error codes:
```json
{
  "results": [
    { "type": "rows", "columns": ["count"], "rows": [[2]] },
    { "type": "rows", "columns": ["id", "name"], "rows": [[1, "alice"]] },
    { "type": "inserted", "count": 1 }
  ],
  "errors": [null, null, null]
}
```

A failed slot:
```json
{
  "results": [null],
  "errors": ["table not found: nonexistent_table"]
}
```

A skipped slot (after a failure when `stop_on_error: true`):
```json
{
  "results": [null],
  "errors": ["skipped"]
}
```

**Error codes** (HTTP-level — only for malformed requests, not statement
failures):

| Code | Status | Meaning |
|------|--------|---------|
| `BATCH_TOO_LARGE` | `400` | More than 256 statements in one request |

Auth: `authorize_sql` is called per statement (honours per-user grants). Auth
DDL (`CREATE USER` / `GRANT` / `REVOKE`) is accepted per-slot via the same
`execute_sql_as` path as `POST /sql`.

---

### `GET /sql/cursor/{cursor_id}` · `DELETE /sql/cursor/{cursor_id}`

Page (or drop) a cursor opened by `POST /sql` with `"cursor": true` (R4).

**Query parameters**: `limit` — rows per page, default 1000, capped at
10 000.

**Response** `200 OK`:
```json
{ "columns": ["id"], "rows": [[1], [2]], "done": false, "remaining": 118000 }
```
The final page reports `"done": true` and the cursor is dropped; fetching
it again returns `404 CURSOR_NOT_FOUND`. Cursors are bound to the creating
principal (`403 CURSOR_FORBIDDEN` otherwise) and expire after
`UNIDB_CURSOR_IDLE_TIMEOUT_SECS` (default 60) of inactivity. `DELETE`
drops a cursor early (`204`).

**Honest cost model:** the engine's executor is synchronous and returns a
fully-materialized result, so the decoded rows stay buffered server-side
for the cursor's lifetime. What a cursor avoids is serializing (and
transferring) one giant JSON array in a single response — every individual
response stays bounded. True incremental executor streaming would be an
engine change, deliberately out of scope (the engine stays sync, §4).

---

### `POST /cypher`

Execute a Cypher subset query (`MATCH ... WHERE ... RETURN ...`) against
graph edge data, atomically.

**Payload**:
```json
{ "query": "MATCH (a)-[:FOLLOWS]->(b) WHERE a.id = 1 RETURN b.id" }
```

**Response** `200 OK`:
```json
{
  "results": [
    { "type": "rows", "columns": ["id"], "rows": [[2], [3]] }
  ]
}
```

---

### `POST /rows`

Insert one raw row. Body is opaque bytes — unidb does not interpret them
(use `/sql` for typed/columnar inserts).

**Payload**: raw bytes, e.g. `curl --data-binary "hello world"`.

**Response** `201 Created`:
```json
{ "row_id": { "page_id": 3, "slot": 0 } }
```

---

### `POST /rows/batch`

Insert up to 10 000 raw rows atomically in one transaction (R4): all inserts
succeed and commit together, or nothing lands. Row payloads are
**base64-encoded** (they are opaque bytes; JSON cannot carry them
verbatim). Every entry is decoded and bounds-checked (32 MiB total decoded)
*before* the first insert runs, so a malformed entry rejects the whole
request up front. Session-aware via `X-Txn-Id`.

**Payload**:
```json
{ "rows": ["aGVsbG8=", "d29ybGQ="] }
```

**Response** `201 Created`:
```json
{ "row_ids": [ { "page_id": 3, "slot": 0 }, { "page_id": 3, "slot": 1 } ] }
```

**Errors**: `400 EMPTY_BATCH` / `400 BAD_BASE64` / `400 BATCH_TOO_LARGE`.

---

### `GET /rows/{page_id}/{slot}`

Read a row back by its `RowId`.

**Response** `200 OK`: raw bytes (`application/octet-stream`), the exact
payload previously inserted.

**Response on missing/deleted row**:
```
HTTP 404
{ "error": "no visible version for row (3, 0)", "code": "NOT_FOUND" }
```

---

### `PUT /rows/{page_id}/{slot}`

Update a row's raw payload.

**Payload**: raw bytes (new payload).

**Response** `200 OK`:
```json
{ "row_id": { "page_id": 3, "slot": 0 } }
```
(`row_id` may differ from the path if the update could not be done
in-place and moved the tuple to a new slot.)

---

### `DELETE /rows/{page_id}/{slot}`

**Payload**: none.

**Response**: `204 No Content` on success.

---

### `POST /edges`

Create a graph edge.

**Payload**:
```json
{
  "from_id": 1,
  "to_id": 2,
  "edge_type": "FOLLOWS",
  "props": { "since": "2024-01-01" }
}
```
`props` is optional and defaults to `{}`.

**Response** `201 Created`:
```json
{ "row_id": { "page_id": 5, "slot": 2 } }
```

---

### `DELETE /edges/{page_id}/{slot}`

**Payload**:
```json
{ "from_id": 1 }
```
(`from_id` is required — edges are keyed/indexed by source vertex, and the
delete path needs it alongside the `RowId` in the URL.)

**Response**: `204 No Content` on success.

---

### `GET /edges/from/{from_id}`

List every outgoing edge from a vertex.

**Response** `200 OK`:
```json
{
  "edges": [
    {
      "row_id": { "page_id": 5, "slot": 2 },
      "to_id": 2,
      "edge_type": "FOLLOWS",
      "props": "{\"since\":\"2024-01-01\"}"
    }
  ]
}
```
Note: `props` here is the raw JSON **text** (not re-parsed), unlike
`/sql`'s `JSON` column handling — `Edge` is serialized directly via
`#[derive(Serialize)]`, not through `dto::literal_to_json`.

---

### `POST /indexes`

Create (or drop, if `kind` is omitted) a secondary index on a column. Not
wrapped in a transaction — mirrors `Engine::set_column_index`'s own
non-transactional signature (a catalog + background-worker operation).

**Payload**:
```json
{ "table": "docs", "column": "embedding", "kind": "Hnsw" }
```
`kind` is one of `"Hnsw"` (only valid on a `VECTOR(n)` column) or
`"FullText"` (only valid on a `TEXT` column). Omit `kind` (or send `null`)
to drop an existing index on that column.

**Response**: `204 No Content` on success.

---

### `GET /indexes/{table}/{column}/status`

Report a column's index status. Since Phase 3 (P3.c) **every** secondary index is
durable and built synchronously as part of `CREATE INDEX` (B-Tree/full-text/edge
as on-disk `DiskBTree`s, the vector index as an on-disk HNSW graph `DiskHnswIndex`), so a present
index is always `"Ready"` — there is no async backfill window. The `Building`
variant is retained for wire compatibility but is no longer produced.

**Response** `200 OK`, if an index exists on that column:
```json
{ "status": "Ready" }
```
or, if no index exists on that column:
```json
{ "status": null }
```

---

### `GET /tables`

> **Superseded (Milestone 18), kept for back-compat.** The documented source of
> truth for introspection is now the SQL-queryable system catalog — `SELECT`
> from `information_schema.tables` / `information_schema.columns` (and
> `table_constraints` / `key_column_usage` / `referential_constraints` /
> `unidb_catalog.indexes`) over `POST /sql`. That catalog exposes primary keys,
> foreign keys, and indexes this flat route never did, and is reachable from
> embed/attach/server alike. See `docs/engine_access_guide.md`. `GET /tables`
> stays for existing clients; new tools should use the catalog.

Schema introspection (S1, studio UI). List every **user** table with its
columns — built from the in-memory catalog, so it is cheap (no heap scan).

Internal engine tables (`__events__`, `__consumers__`, `__edges__`,
`__lobs__` — everything under the reserved `__…__` naming convention) are
**omitted**. There is deliberately **no `row_count`** in v1: a row count is a
full scan, out of scope for a lightweight introspection call. Logically dropped
columns (`ALTER TABLE DROP COLUMN`) are excluded, mirroring `SELECT *`.

**Payload**: none.

**Response** `200 OK` — a JSON array, sorted by table name for determinism:
```json
[
  {
    "name": "docs",
    "columns": [
      { "name": "id", "type": "int", "nullable": true, "index": null },
      { "name": "embedding", "type": "vector(4)", "nullable": true, "index": "hnsw" }
    ]
  },
  {
    "name": "users",
    "columns": [
      { "name": "id", "type": "int", "nullable": false, "index": null },
      { "name": "email", "type": "text", "nullable": false, "index": null }
    ]
  }
]
```

Per column:
- `type` — a human-readable type name: `int`, `text`, `bool`, `json`, `float`,
  `uuid`, `bytea`, `date`, `time`, `timestamp`, `vector(<n>)`,
  `decimal(<p>,<s>)`. (This is the REST vocabulary, owned by `server/dto.rs`;
  it is intentionally decoupled from the engine's on-disk `ColumnType` enum.)
- `nullable` — `false` iff the column is `NOT NULL` or `PRIMARY KEY`.
- `index` — the column's secondary-index kind (`btree`, `hnsw`, `fulltext`,
  `csr`) or `null` if unindexed. `hnsw` denotes the durable on-disk HNSW graph
  index (`DiskHnswIndex`, item 63; recall@10 ≥ 0.95); see `catalog::IndexKind`.

**Errors**: same as every data-plane route — `401 UNAUTHORIZED` without a valid
bearer token, `500 INTERNAL_ERROR` if the engine is unavailable. No route-specific
error codes.

---

### `POST /tables/{table}/bulk` (item 32)

Bulk-insert NDJSON rows into a table in **one transaction** — begin once,
prepare the `INSERT` SQL once, loop over rows, commit once. This amortizes
the per-row HTTP + per-statement fsync overhead (~1.5 ms/row on the `/sql`
one-call-per-row path). **Measured (release, reproducible via the `#[ignore]`d
`bulk_throughput_measurement` test): ~12k–31k rows/sec** — index-dependent
(no secondary index amortizes toward ~31k at 200k rows; a B-tree index costs
~12k–17k and degrades as it grows), a ~20–50× win over the ~640 rows/sec
per-row path. This is **below** the 50k–200k aspiration; the path there
(channel-streamed body + lower-level insert) is a filed follow-up. See
`PROGRESS.md` item-32 entry.

**Content-Type**: `application/x-ndjson` — one JSON object per line.
All rows must share the same key set. The first row's key-iteration order
becomes the INSERT column order; subsequent rows look up values by name
(field order within an object does not matter). Missing keys become `NULL`.

**Request**:
```
POST /tables/customers/bulk
Authorization: Bearer <token>
Content-Type: application/x-ndjson

{"id":1,"name":"Alice","score":1.5,"active":true}
{"id":2,"name":"Bob","score":2.0,"active":false}
...
```

**Response**: `200 OK`
```json
{ "inserted": 200000, "errors": 0, "elapsed_ms": 2340 }
```

**Error behaviour**: any error (malformed NDJSON, type mismatch, table not
found, constraint violation) rolls back the **whole batch** atomically.
A partial insert is never left visible.

**Atomicity note**: one transaction holds the undo log and pins the vacuum
horizon for its entire duration. Very large batches (millions of rows) have
a corresponding memory/WAL footprint. A `?chunk=N` commit-every-N mode is
a documented follow-up for callers that prefer throughput over strict batch
atomicity.

**Body size**: the server buffers up to 512 MiB. Larger payloads are
rejected with `400 BODY_TOO_LARGE`.

**Error codes** (in addition to standard engine codes):

| HTTP | code | meaning |
|------|------|---------|
| 400 | `MALFORMED_NDJSON` | body is not valid NDJSON (includes line number) |
| 400 | `BODY_TOO_LARGE` | body exceeds 512 MiB limit |
| 400 | `INVALID_TABLE_NAME` | table name contains characters outside `[A-Za-z_][A-Za-z0-9_]*` |
| 400 | `INVALID_COLUMN_NAME` | a JSON key contains invalid SQL identifier characters |
| 400 | `EMPTY_ROW` | first row has no keys |

---

### `POST /tables/{table}/events`

Opt a table into event capture (M4). From this point on, every
INSERT/UPDATE/DELETE on `table` also durably writes a row to the internal
`__events__` table under the same transaction. Required before
`GET /events/subscribe` or `POST /events/ack` return anything meaningful
for that table.

**Payload**: none.

**Response**: `204 No Content` on success.

---

### `GET /tables/{table}/events`

Query CDC status for a table (item 33).

**Response `200 OK`**:
```json
{ "enabled": true }
```
Returns `{ "enabled": false }` when the table exists but CDC is off.
Returns `404 TABLE_NOT_FOUND` if the table does not exist.

---

### `DELETE /tables/{table}/events`

Disable CDC on a table (item 33). Already-captured events in `__events__`
are **not** drained — they remain until consumed and vacuumed. Only future
writes stop emitting events.

**Idempotency decision (item 33):** Returns `204` even when CDC was already
off. This matches standard REST disable semantics and avoids the client
needing a prior `GET` to avoid a spurious error.

**Response**: `204 No Content` on success.  
**Error**: `404 TABLE_NOT_FOUND` if the table does not exist.

---

### `GET /events/head`

Return the current highest committed `seq` in `__events__` without opening a
stream (item 33). Useful for "start from now" positioning — avoid replaying
the full event history when subscribing fresh.

**Response `200 OK`**:
```json
{ "seq": 134937 }
```
Returns `{ "seq": 0 }` if no events have ever been written. O(1) via the
durable `__events__.seq` B-tree index — no heap scan.

---

### `GET /events/subscribe`

Server-Sent Events stream of new events on tables that have event capture
enabled. **This is a server poll loop, not WAL-level push** — the server
calls `poll_events` on an interval and forwards results as SSE frames; see
`sse.rs`'s module doc for the cost model (`N subscribers × poll interval ×
poll_events's own linear-in-table-size cost`, quantified in the M5
benchmark table in `PROGRESS.md`).

Two modes (M20 E1), selected by whether `consumer` is present:

- **Durable consumer** (`consumer` set): at-least-once, resumes from that
  consumer's durable acked offset. Un-acked events are re-yielded until acked.
- **Ephemeral live-tail** (`consumer` omitted): at-most-once browser tail, no
  durable offset written. Resumes strictly past the standard `Last-Event-ID`
  reconnect header (each frame carries `id: <seq>`), else `from_seq`, else `0`.

**Query parameters**:

| Param | Required | Default | Meaning |
|---|---|---|---|
| `consumer` | no | — | Durable consumer name → at-least-once mode. Omit for the ephemeral live-tail mode |
| `from_seq` | no | — | Ephemeral mode only: start strictly after this offset (offset scrubbing / replay-from-offset). Overridden by the `Last-Event-ID` header |
| `table` | no | — | Deliver only events for this table |
| `limit` | no | `100` | Max events fetched per poll tick |
| `interval_ms` | no | `500` | Poll interval in milliseconds |

**Request headers**: `Last-Event-ID: <seq>` (ephemeral mode) — standard SSE
reconnect cursor; the stream resumes strictly after `<seq>`. Wins over
`from_seq`.

**Response**: `200 OK`, `Content-Type: text/event-stream`, one frame per
new event:
```
id: 17
event: insert
data: {"seq":17,"xid":42,"table_name":"orders","op":"insert","payload":{"id":1,"total":9.99}}

```
Acks are **not** sent over this connection — call `POST /events/ack`
separately (below) once events are durably processed. Downstream fan-out
(webhooks/rooms with retry + dead-letter) is the `unidb-dispatch` crate
(M20 E2), not an engine route — see `docs/engine_access_guide.md §8`.

---

### `POST /events/ack`

Durably advance a consumer's offset so already-acked events are never
redelivered on a future subscribe/poll.

**Payload**:
```json
{ "consumer": "billing-worker", "up_to_seq": 17 }
```

**Response**: `204 No Content` on success.

---

### `POST /events/vacuum`

Reclaim fully-consumed events (R3): deletes every `__events__` row whose
`seq` is at or below the **minimum** acked offset across *all* registered
consumers — the M4 slow-consumer durability contract (an event outlives
vacuum until its slowest consumer has durably acked past it; with no
consumer registered, nothing is reclaimable).

**Payload**: none.

**Response** `200 OK`:
```json
{ "reclaimed": 17 }
```

---

### `PUT /tables/{table}/rls`

Attach a row-level-security policy to a table (R3), as a **SQL predicate
string** — the same AND-only comparison subset `WHERE` accepts, parsed by
the ordinary SQL parser (chosen over a JSON policy DSL so there is exactly
one grammar). The policy is AND-rewritten into every query on the table.
**Superuser-gated** (P6.e semantics): RLS is an access-control boundary.

**Payload**:
```json
{ "predicate": "tenant_id = 7" }
```

**Response**: `204 No Content`. `400 SQL_PARSE_ERROR`/`SQL_UNSUPPORTED`
for a malformed or non-AND-only predicate (e.g. `OR`), `404
TABLE_NOT_FOUND`, `403 PERMISSION_DENIED` for a non-superuser.

---

### `POST /admin/flush`

Force the WAL durable, then flush every dirty page (`Engine::flush`,
previously test-only; R3). **Superuser-gated** — an I/O-amplification
lever, not a data-plane operation. In open/bootstrap mode (no registered
users) any authenticated principal passes, matching every other P6.e gate.

**Payload**: none.

**Response**: `204 No Content`.

---

### `POST /checkpoint`

Trigger `Engine::checkpoint()` manually: flush dirty pages, write a
checkpoint WAL record, persist `next_xid`, truncate the WAL. Operational
route — same auth as everything else in v1 (no admin-only scope).

**Payload**: none.

**Response**: `204 No Content` on success.

---

### `GET /stats` (P6.g + item 21, enriched by items 26/27/29)

A `pg_stat_*`-style activity snapshot. Item 21 enriches it with production-grade
metrics captured lock-free at existing chokepoints (per-statement-kind latency,
WAL-fsync cost, buffer-pool efficiency, lock contention, the vacuum-horizon-age
gauge, per-table page counts, and worker-governance utilization). The same
boundary — no new endpoint, per the Milestone-18 rule — later grew two more
fields: item 27 broke `dead_tuple_estimate`/`live_tuple_estimate` out **per
table** (in addition to the engine-global totals, which remain for backward
compatibility), and item 29 added a `subscription_lag` array for CDC/event
consumers.

**Response** `200 OK`:
```json
{
  "commits": 42, "aborts": 3, "checkpoints": 1,
  "active_transactions": 0, "wal_bytes": 81920,
  "replication_slots": 1, "max_replication_lag": 128,
  "data_pages": 37, "recent_slow_queries": [{"sql": "...", "micros": 4210}],
  "autovacuums": 2, "dead_tuple_estimate": 5, "live_tuple_estimate": 40,
  "last_autovacuum_epoch_secs": 1752345600,
  "statement_latency": {
    "insert": {"count": 50, "p50_us": 32, "p99_us": 256, "mean_us": 41},
    "update": {"count": 1, "p50_us": 64, "p99_us": 64, "mean_us": 60},
    "delete": {"count": 1, "p50_us": 64, "p99_us": 64, "mean_us": 55},
    "select": {"count": 3, "p50_us": 16, "p99_us": 32, "mean_us": 18}
  },
  "wal_fsyncs": 12, "wal_fsync_latency": {"count": 12, "p50_us": 512, "p99_us": 2048, "mean_us": 640},
  "bufferpool": {"hits": 980, "misses": 40, "evictions": 3, "hit_ratio": 0.9607},
  "locks": {"waits": 0, "deadlocks": 0, "wait": {"count": 0, "p50_us": 0, "p99_us": 0, "mean_us": 0}},
  "horizon_age_secs": 0.0,
  "parallel_workers": {"global_max": 8, "available": 8, "parallel_scans": 0, "workers_granted": 0, "serial_fallbacks": 0},
  "tables": [{"name": "t", "pages": 3, "dead_tuple_estimate": 2, "live_tuple_estimate": 118}],
  "subscription_lag": [
    {"consumer": "billing-worker", "offset": 41, "max_seq": 42, "lag_events": 1, "oldest_unconsumed_ts_ms": 1752345600123, "lag_seconds": 3.4}
  ],
  "open_txn_sessions": 0, "open_cursors": 0, "idle_reaper_aborts": 0
}
```
`open_txn_sessions` / `open_cursors` / `idle_reaper_aborts` are server-layer
gauges (R1/R4 + item 21) added alongside the engine counters — the engine can't
see HTTP sessions. Percentiles are log-bucket **estimates** (the `le`
convention).

**Per-table vs. engine-global (item 27).** Each `tables[]` entry now carries
its own `dead_tuple_estimate`/`live_tuple_estimate` (V1/V2/V3, autovacuum's
per-table trigger), correcting an earlier stated limitation that this pressure
was only ever available engine-wide — it wasn't split out per table until
item 27 shipped. The flat top-level `dead_tuple_estimate`/`live_tuple_estimate`
fields remain and still cover **raw-CRUD heap** writes (`Engine::insert`/
`update`/`delete` with no table name), which have no per-table home to
attribute to; SQL-path writes are reflected in both the per-table and the
engine-global numbers. Per-table counters reset to `0` on reopen (approximate
by design, like Postgres `n_dead_tup`) and refresh at the next vacuum pass for
that table.

**Subscription lag (item 29).** `subscription_lag` has one entry per consumer
that has ever called `POST /events/ack`; a consumer that has never acked is
absent, not zeroed. `lag_events` is `max_seq − offset` across every
event-enabled table; `lag_seconds` is the age of the oldest unacknowledged
event. The same numbers are queryable as an ordinary table via
`SELECT * FROM unidb_catalog.subscription_lag` and published as Prometheus
gauges (see `GET /metrics` below) — alert on `lag_seconds`, not `lag_events`,
since event size/rate varies per table.

The full metric ↔ widget map is `docs/engine_access_guide.md` §10
(widget-traceability table).

---

### `GET /stats/history` (item 34)

Returns the engine's 300-point ring buffer of timestamped stats snapshots, with
server-side rate fields computed from consecutive entries — the Studio
Observability tab prefills its charts from this endpoint on mount so they
survive page reloads.

**Query params:**

| param | default | max | meaning |
|-------|---------|-----|---------|
| `points` | 60 | 300 | number of snapshots to return (most recent) |
| `interval_ms` | 5000 | — | resolution hint; echoed back in response |

Points are oldest-first. The ring is populated by a background thread every 5 s
once the server starts (`Engine::open()` alone does **not** start the ticker,
so deterministic tests that use bare `Engine::open()` see an empty ring until
they call `Engine::capture_stats_point()` manually).

**Response** `200 OK`:
```json
{
  "interval_ms": 5000,
  "points": [
    {
      "t": 1752350400000,
      "commits": 42, "aborts": 3, "active_transactions": 0, "wal_bytes": 81920,
      "commits_per_sec": 1.4, "wal_bytes_per_sec": 2048.0,
      "bufferpool_hit_ratio": 0.96
    }
  ]
}
```

`commits_per_sec` / `wal_bytes_per_sec` are derived from the delta between
consecutive ring entries. The first point in the returned slice always has both
rates as `0.0` (no predecessor). An empty `points: []` is returned on a fresh
engine and is not an error.

---

### `PUT /config/slow_query_threshold_ms` (item 34)

**Superuser-gated** (same gate as `PUT /tables/{table}/rls` and
`POST /admin/flush`). Updates the slow-query threshold at runtime without a
server restart.

**Request body:**
```json
{ "threshold_ms": 100 }
```

`threshold_ms: 0` disables slow-query logging. Positive values enable it: any
SQL statement whose wall-clock exceeds the threshold is logged via
`tracing::warn` (target `unidb::slow_query`) and appended to the bounded ring
surfaced by `GET /stats` → `recent_slow_queries`.

The threshold can also be set at server startup via the `UNIDB_SLOW_QUERY_MS`
environment variable (absent or `0` = disabled, the default).

**Response** `204 No Content`.

---

### `GET /logs` (item 22)

**Superuser-gated.** A bounded, cursor-paged tail over the rotated JSON log
files the server writes (`unidb.log.YYYY-MM-DD`). This is **not** a log
database — it is a filtered *reverse read of the files*, so a real deployment
still ships those files to CloudWatch/Datadog (see `ops_runbook.md`); the
endpoint is the local/single-node convenience (and the studio Logs tab's
backend).

**Query params** (all optional):

| param    | meaning |
|----------|---------|
| `level`  | minimum severity — `ERROR` > `WARN` > `INFO` > `DEBUG` > `TRACE`; a line at or above it passes |
| `since`  | inclusive lower bound on the line's RFC3339-UTC `timestamp` (lexical compare) |
| `until`  | inclusive upper bound on the `timestamp` |
| `q`      | case-sensitive substring the raw line must contain (e.g. a `request_id`) |
| `cursor` | opaque resume token from a prior page's `next_cursor` |
| `limit`  | page size, **clamped to 500** |

**Response** `200 OK`:
```json
{
  "logs": [ { "timestamp": "...", "level": "INFO", "request_id": "req-...", "...": "..." } ],
  "returned": 42,
  "scanned": 137,
  "truncated": false,
  "next_cursor": "b64-opaque-or-null"
}
```
- `logs` are newest-first, each the parsed JSON line (or `{"raw": "..."}` for a
  non-JSON line, which only a bare `q` passes).
- `next_cursor` is `null` at the end; otherwise pass it back to fetch the next
  (older) page.
- **Bounds that keep a multi-GB log directory safe:** at most 500 lines are
  returned, at most 50 000 are *examined* per request, and files are read from
  the end backward one block at a time — never loaded whole. `truncated: true`
  means the per-request scan budget stopped the walk before the page filled
  (there is more behind `next_cursor`), not that the corpus ended.

**Correlation (L2):** every request is stamped with a `request_id` (echoed in
the `x-request-id` response header). It appears on the request's app-log lines,
its slow-query log line, and its `audit.log` entries (alongside `txn_id`), so
one request's lines are retrievable across all three by that id.

---

### Replication (P6.b)

- `POST /replication/slots` — create a slot. Body `{"name": "...", "sync": false}`.
  `201 Created` with `{"name","restart_lsn","kind"}`.
- `GET /replication/slots` — list slots: `{"slots": [...]}`.
- `DELETE /replication/slots/{name}` — drop a slot. `204`.
- `POST /replication/slots/{name}/advance` — a consumer confirms it applied up to
  an LSN. Body `{"lsn": <n>}`. `204`.
- `GET /replication/stream?from_lsn={n}` — ship WAL records after `from_lsn` as
  `application/octet-stream`; the primary's tail LSN is in the `x-unidb-tail-lsn`
  response header. Decode with `wal::decode_stream` and apply via a replica.

A bad slot request (duplicate/unknown name) returns `400 REPLICATION_ERROR`.

---

### Per-user authorization (P6.e)

`POST /sql` also accepts the auth DDL `CREATE USER|ROLE`, `GRANT`, `REVOKE`
(superuser only). The JWT `sub` claim is the acting username; a token with no
`sub` is the implicit superuser. With no users registered the server is in open
mode (backward compatible); once users exist, a missing privilege returns
`403 PERMISSION_DENIED`. All auth DDL + named-user decisions are written to
`audit.log`.

See [Authorization — roles, grants, and RLS](#authorization--roles-grants-and-rls-item-24) below for the
full SQL DDL surface added by item 24.

**TLS (P6.f):** set `UNIDB_TLS_CERT`/`UNIDB_TLS_KEY` (PEM) to serve HTTPS.

---

### Authorization — roles, grants, and RLS (item 24)

All auth DDL is sent as normal SQL via `POST /sql`. Auth DDL requires a **superuser** JWT
(a token whose `sub` maps to a role with `SUPERUSER`, or a token with no `sub`). A
non-superuser issuing auth DDL or schema DDL receives `403 PERMISSION_DENIED`.

#### Roles

```sql
-- Create a role (regular or superuser)
CREATE ROLE analyst;
CREATE ROLE admin SUPERUSER;

-- Drop a role
DROP ROLE analyst;
```

#### Grants

```sql
-- Grant individual privileges on a table
GRANT SELECT ON orders TO analyst;
GRANT INSERT ON orders TO analyst;
GRANT UPDATE ON orders TO analyst;
GRANT DELETE ON orders TO analyst;

-- Grant all privileges at once
GRANT ALL ON orders TO analyst;

-- Revoke privileges
REVOKE SELECT ON orders FROM analyst;
REVOKE ALL ON orders FROM analyst;
```

A non-superuser executing a SQL statement on a table for which they lack the corresponding
privilege receives `403 PERMISSION_DENIED`.

#### Row-level security (RLS) policies

```sql
-- Policy enforced on SELECT (AND-rewrite via apply_rls)
CREATE POLICY my_select_policy ON orders FOR SELECT USING (tenant_id = 42);

-- Policy enforced on INSERT (per-row check in exec_insert)
CREATE POLICY my_insert_policy ON orders FOR INSERT USING (status = 'pending');

-- Policy for UPDATE or DELETE
CREATE POLICY my_update_policy ON orders FOR UPDATE USING (owner_id = 99);
CREATE POLICY my_delete_policy ON orders FOR DELETE USING (archived = true);

-- ALL applies the policy to every operation (INSERT enforcement + SELECT/UPDATE/DELETE rewrite)
CREATE POLICY all_ops_policy ON orders FOR ALL USING (tenant_id = 42);

-- Drop a policy
DROP POLICY my_select_policy ON orders;
```

INSERT policies are enforced per-row in `exec_insert` via `insert_policy`. SELECT, UPDATE,
and DELETE policies are applied as a predicate AND-rewrite via `apply_rls` at query planning
time, regardless of which route invoked `execute_sql`. Policies persist across server restart
(catalog-stored). Multiple policies on the same table and operation are combined with AND.

#### Enforcement on server routes

| Route | Enforcement |
|---|---|
| `POST /sql` (one-shot) | `authorize_sql(user, sql)` pre-checks privilege before execution |
| `POST /sql` (session) | `authorize_sql(user, sql)` pre-checks privilege before execution |
| `POST /tables/{name}/bulk` | `check_table_grant(user, table, Insert)` checked before reading body |
| `POST /auth/preview` | `ensure_superuser(caller)` gate; then `execute_sql_as(as_role, ...)` with full RLS |
| `GET /rows/*`, `PUT /rows/*` etc. | Intentionally unenforced — pre-SQL era routes with no table name |

#### `current_user` in RLS policies (item-24 Z6)

RLS policies may reference `current_user` (bare keyword) as a dynamic placeholder for the
authenticated identity executing the query:

```sql
-- owner column must equal the calling user
CREATE POLICY tenant_isolation ON posts FOR SELECT USING (owner = current_user);
CREATE POLICY own_rows_only ON items FOR INSERT USING (owner = current_user);
```

At query planning time, `current_user` is substituted with the JWT `sub` claim of the caller
before the policy predicate is AND-rewritten into the plan. The bare keyword form (`current_user`,
no parentheses) is required — `current_user()` with parentheses is not accepted. Superusers and
the embedded API path (`sub` absent) bypass `current_user`-containing policies entirely.

#### `POST /auth/preview` — preview RLS as a named role (item-24 Z6)

Execute SQL as a named role and return the result filtered by that role's RLS policies.
Intended for administrative tooling (e.g. the Auth tab in the Studio UI) to preview exactly
which rows a given user will see.

**Auth:** requires a **superuser** JWT. Non-superusers receive `403 Forbidden`.

**Request** `POST /auth/preview`:
```json
{
  "as_role": "alice",
  "sql": "SELECT id, owner FROM posts"
}
```

| Field | Type | Required | Description |
|---|---|---|---|
| `as_role` | string | yes | Username to impersonate. Must be a registered user. |
| `sql` | string | yes | Single SQL statement to execute under that identity. |

**Response** `200 OK` — same shape as `POST /sql`:
```json
{
  "type": "rows",
  "columns": ["id", "owner"],
  "rows": [[1, "alice"]]
}
```

**Error codes:**

| Status | Condition |
|---|---|
| `403 Forbidden` | Caller is not a superuser. |
| `400 Bad Request` | Malformed or unsupported SQL. |
| `500 Internal Server Error` | Engine error. |

**Example curl:**
```bash
curl -s -X POST http://localhost:7777/auth/preview \
  -H "Authorization: Bearer $SUPERUSER_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"as_role": "alice", "sql": "SELECT id, owner FROM posts"}'
```

#### Catalog virtual relations

The current roles, grants, and policies are queryable as virtual relations:

```sql
SELECT * FROM unidb_catalog.roles;
SELECT * FROM unidb_catalog.grants;
SELECT * FROM unidb_catalog.policies;
```

These are read-only virtual tables synthesized by the executor from the in-memory `RoleStore`
(which is itself loaded from the persisted catalog at engine open). They do not correspond to
heap pages on disk.

#### Open-mode compatibility

With no roles registered, the server runs in **open mode**: all validly-authenticated users
have full access to all tables. Open mode is the default and maintains full backward
compatibility with deployments that do not use per-user authorization. Once any role or grant
is registered, the engine enforces privileges on all subsequent requests.

---

### `GET /metrics`

Prometheus text exposition format. The only route with no JWT requirement.

**Response** `200 OK`, `Content-Type: text/plain; ...`:
```
# HELP axum_http_requests_total ...
# TYPE axum_http_requests_total counter
axum_http_requests_total{method="POST",path="/sql",status="200"} 12
...
unidb_jwt_verify_seconds_sum 0.000012
unidb_sse_poll_cycles_total 340
unidb_sse_events_delivered_total 17
# item 21: engine metrics republished from stats() on each scrape
unidb_commits_total 42
unidb_statement_latency_p99_us{kind="insert"} 256
unidb_bufferpool_hit_ratio 0.9607
unidb_wal_fsyncs_total 12
unidb_horizon_age_seconds 0
unidb_parallel_worker_budget 8
unidb_table_pages{table="t"} 3
unidb_open_txn_sessions 0
# item 27: per-table vacuum accounting — engine-global scalars only on this
# facade (see the note under GET /stats above; the per-table breakdown is
# JSON-only via tables[].dead_tuple_estimate/live_tuple_estimate)
unidb_dead_tuple_estimate 5
unidb_live_tuple_estimate 40
# item 29: per-consumer CDC/event lag — alert on the seconds gauge
unidb_subscription_lag_events{consumer="billing-worker"} 1
unidb_subscription_lag_seconds{consumer="billing-worker"} 3.4
```
Every metric name (and the widget it drives) is documented in
`docs/engine_access_guide.md` §10 (grown since item 21's original enrichment
by items 27 and 29, on the same boundary — no new endpoint).

---

## Error codes

Every error maps through `src/server/error.rs::map_status`. Client-facing
`DbError` variants are listed individually and exhaustively; everything
else (low-level storage/recovery errors a well-formed request should never
trigger) falls into one grouped 500.

Server-layer codes (transaction sessions R1, cursors/batch R4) are emitted
by `server/error.rs`'s `ApiError` directly, not by a `DbError` variant.

> **Correction (R-enrichment docs pass, 2026-07-11):** this table had gone
> stale — `DEADLOCK`, `QUERY_TIMEOUT`/`QUERY_CANCELLED`,
> `REPLICATION_ERROR`, `AUTHZ_ERROR`, and `PERMISSION_DENIED` shipped with
> P5.d/P5.f/P6.b/P6.e but were only mentioned in prose (or not at all).
> They are listed properly below.

| HTTP status | `code` | Triggered by |
|---|---|---|
| 404 | `TABLE_NOT_FOUND` | Referenced table doesn't exist |
| 404 | `COLUMN_NOT_FOUND` | Referenced column doesn't exist |
| 404 | `NOT_FOUND` | Row has no MVCC-visible version (deleted/never existed) |
| 404 | `TXN_NOT_FOUND` | Unknown/finished/reaped transaction session id (R1) |
| 404 | `CURSOR_NOT_FOUND` | Unknown/exhausted/expired cursor id (R4) |
| 409 | `TABLE_ALREADY_EXISTS` | `CREATE TABLE` on an existing name |
| 409 | `WRITE_CONFLICT` | Concurrent write conflict (lock manager) |
| 409 | `SERIALIZATION_FAILURE` | Snapshot-isolation / SSI abort-on-conflict |
| 409 | `DEADLOCK` | Wait-for-graph deadlock victim (P5.d) |
| 409 | `TXN_BUSY` | Second concurrent request on one session (R1) |
| 409 | `UNIQUE_VIOLATION` | Write duplicated a `UNIQUE`/`PRIMARY KEY` value (M11) |
| 408 | `QUERY_TIMEOUT` / `QUERY_CANCELLED` | Per-query time budget / cancellation (P5.f) |
| 403 | `TXN_FORBIDDEN` | Session belongs to a different JWT principal (R1) |
| 403 | `CURSOR_FORBIDDEN` | Cursor belongs to a different JWT principal (R4) |
| 403 | `PERMISSION_DENIED` | Missing per-user privilege / superuser gate (P6.e) |
| 400 | `SQL_PARSE_ERROR` | Malformed SQL |
| 400 | `SQL_PLAN_ERROR` | SQL that parses but doesn't plan (e.g. bad rewrite) |
| 400 | `SQL_UNSUPPORTED` | Valid SQL outside unidb's supported subset |
| 400 | `NOT_NULL_VIOLATION` | Write left a `NOT NULL`/PK column NULL (M11) |
| 400 | `CHECK_VIOLATION` | Write failed a `CHECK` constraint (M11) |
| 400 | `FOREIGN_KEY_VIOLATION` | `FOREIGN KEY` references a table that doesn't exist (M11) |
| 400 | `TXN_NOT_ACTIVE` | Operation on a transaction that isn't active |
| 400 | `TXN_ALREADY_FINISHED` | Operation on an already committed/aborted txn |
| 400 | `BAD_PAGE_SIZE` | Invalid page size at open |
| 400 | `BAD_TXN_ID` | Malformed `X-Txn-Id` header (R1) |
| 400 | `DDL_IN_SESSION` | Catalog/auth DDL inside a transaction session (R1) |
| 400 | `ISOLATION_IN_SESSION` | `isolation` field on a session statement (R1/R2) |
| 400 | `BAD_REQUEST_BODY` | Malformed `POST /txn/begin` body (R1) |
| 400 | `CURSOR_NOT_ROWS` | Cursor mode on a non-rows statement (R4) |
| 400 | `EMPTY_BATCH` / `BAD_BASE64` / `BATCH_TOO_LARGE` | Invalid `POST /rows/batch` payload (R4) |
| 400 | `REPLICATION_ERROR` | Bad slot request — duplicate/unknown name (P6.b) |
| 400 | `AUTHZ_ERROR` | Malformed users/roles/GRANT statement (P6.e) |
| 401 | `UNAUTHORIZED` | Missing/malformed/wrong-signature/expired JWT |
| 503 | `DURABILITY_FAILURE` | An `fsync`/`msync` failed (P1.b, fsyncgate); the engine can no longer guarantee durability and must be restarted (session is poisoned) |
| 500 | `INTERNAL_ERROR` | I/O, checksum, WAL corruption, control-file corruption, catalog corruption, buffer pool exhaustion, or an unavailable engine (`EngineUnavailable`) |

---

---

## Storage service routes (item 31)

Seven routes surface the `unidb-storage` app-layer crate as protected REST
endpoints. All require a JWT bearer token. All return
`503 {"error":"…","code":"STORAGE_NOT_AVAILABLE"}` when storage is not
configured (`STORAGE_BACKEND` env var absent or init failed at startup) — the
server boots cleanly without storage.

### 503-when-unconfigured contract

Every handler calls `require_storage` before touching the service. If
`AppState::storage` is `None`, the response is always:

```
HTTP/1.1 503 Service Unavailable
{"error":"storage service is not configured (STORAGE_BACKEND not set or init failed)","code":"STORAGE_NOT_AVAILABLE"}
```

No 500, no panic, regardless of the request body or path params.

### C1 — List buckets

```
GET /storage/buckets
Authorization: Bearer <token>

→ 200 { "buckets": [ { "name": "…", "created_by": "…"|null, "created_at_ms": N } ] }
→ 503 STORAGE_NOT_AVAILABLE
```

### C2 — Create bucket

```
POST /storage/buckets
Authorization: Bearer <token>
Content-Type: application/json
{ "name": "my-bucket" }

→ 201 (empty body)
→ 503 STORAGE_NOT_AVAILABLE
```

### C3 — Delete bucket

```
DELETE /storage/buckets/{name}
Authorization: Bearer <token>

→ 204 (empty body)
→ 409 { "error":"bucket 'name' still contains objects", "code":"BUCKET_NOT_EMPTY" }
→ 503 STORAGE_NOT_AVAILABLE
```

Returns 409 if the bucket has any object rows. Delete all objects first.

### C4 — List objects (virtual-folder aware)

```
GET /storage/{bucket}/objects[?prefix=photos/&delimiter=/]
Authorization: Bearer <token>

→ 200 {
    "objects":  [ { "object_key":"…", "size":N, "etag":"…"|null, "content_type":"…"|null,
                    "status":"ready"|"pending", "tier":"inline"|"s3",
                    "created_at_ms":N } ],
    "prefixes": [ "photos/vacation/" ]   // virtual folders
  }
→ 503 STORAGE_NOT_AVAILABLE
```

With `prefix` + `delimiter`, objects whose key suffix (after the prefix) contains
the delimiter are folded into `prefixes` (virtual folders); the rest appear in
`objects`. Standard S3-style listing semantics.

### C5 — Put object (inline or presigned)

```
PUT /storage/{bucket}/objects/{*key}
Authorization: Bearer <token>
Content-Type: <mime-type>           (optional)
<body bytes>

→ 201 { "tier":"inline", "size":N, "etag":"…"|null }   // body.len() < inline_threshold
→ 200 { "presigned_put_url":"https://…", "storage_key":"…" }  // body.len() >= threshold
→ 503 STORAGE_NOT_AVAILABLE
```

The split point is `StorageConfig::inline_threshold` (default 1 MiB). When the
body is below threshold, bytes are stored as an engine LOB in one transaction
(response 201). When at or above threshold, a pending metadata row is created and
a presigned PUT URL is returned (response 200); the client must PUT the bytes
directly to that URL.

### C6 — Delete object

```
DELETE /storage/{bucket}/objects/{*key}
Authorization: Bearer <token>

→ 204 (empty body)
→ 404 STORAGE_NOT_FOUND
→ 503 STORAGE_NOT_AVAILABLE
```

### C7 — Presigned GET URL

```
GET /storage/{bucket}/presign/{*key}
Authorization: Bearer <token>

→ 200 { "presigned_get_url": "https://…" }
→ 404 STORAGE_NOT_FOUND
→ 503 STORAGE_NOT_AVAILABLE
```

Returns a time-limited URL for direct browser/client download without exposing
app credentials.

### Storage error codes

| HTTP | code | meaning |
|------|------|---------|
| 503 | `STORAGE_NOT_AVAILABLE` | storage service not configured |
| 404 | `STORAGE_NOT_FOUND` | bucket or object does not exist |
| 409 | `BUCKET_NOT_EMPTY` | delete-bucket blocked by existing objects |
| 503 | `STORAGE_CONFIG_ERROR` | backend config error (bad env vars) |
| 502 | `OBJECT_STORE_ERROR` | upstream S3/MinIO error |

---

## Known limitations

Formerly-listed v1 gaps now closed by the REST-enrichment work (item 12):
multi-request **transaction sessions** (R1), **RLS-over-REST** (R3),
`vacuum_events`/`flush` routes (R3), batch insert + large-result cursors
(R4). TLS termination shipped earlier with P6.f.

Still out of scope (deliberate, not oversights): gRPC / a Postgres wire
protocol (parked), server-side connection pooling, cursor results that
stream incrementally from the executor (the engine is sync; cursors buffer
decoded rows server-side — see the cursor cost model above), and session
support in the Rust attach client (below).

---

## Rust attach client

`unidb-attach` (M8) is a Rust crate wrapping the one-shot routes above in
blocking method calls (`AttachClient::execute_sql`, `insert`,
`create_edge`, `edges_from`, `set_column_index`, `enable_events`, etc.) —
no new wire format, just `reqwest::blocking` + the same JSON shapes
documented in this file. It stays **one-shot**: it does not yet expose the
R1 transaction sessions (an optional follow-up — the wire surface is just
the `X-Txn-Id` header), nor the newer R3/R4 routes (`/events/vacuum`,
`/tables/{table}/rls`, `/admin/flush`, `/rows/batch`, `/sql` cursors) or
M10 heap `vacuum` (which still has no route). See the repo root
[`README.md`](../README.md#rust-attach-client-unidb-attach-m8) and
[`unidb-attach/src/lib.rs`](../unidb-attach/src/lib.rs).
