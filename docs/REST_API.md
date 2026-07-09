# unidb REST API Reference

Covers the optional `unidb-server` binary (M5, gated behind the `server`
Cargo feature). Source of truth for this document: `src/server/router.rs`,
`handlers.rs`, `dto.rs`, `auth.rs`, `sse.rs`, `error.rs`.

This is a thin HTTP wrapper over the embedded `Engine`. Every mutating
route runs exactly one `begin -> execute -> commit-or-abort` cycle on a
single dedicated writer thread (`src/server/engine_handle.rs`; see
`CLAUDE.md` §2 for the overall layer stack). It is **not** a
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
- **Transactions**: nearly every route is a single, complete, self-contained
  transaction. `POST /txn/begin` exists for introspection/debugging only —
  there is no way to later `commit`/`abort` that `xid` over a separate HTTP
  request. Multi-statement atomicity is available today via one
  `;`-separated `/sql` body, not via separate begin/commit calls.

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

Any validly-signed, unexpired token grants access to **every** data-plane
route alike — there is no role/scope claim distinction in v1. Missing,
malformed, wrong-signature, or expired tokens all return:

```
HTTP 401 Unauthorized
{ "error": "invalid token: ExpiredSignature", "code": "UNAUTHORIZED" }
```

`GET /metrics` is the one route that never requires a token (Prometheus
scrapers don't carry app bearer tokens — firewall it at the network layer
in production instead).

---

## Routes

### `POST /txn/begin`

Open a transaction for introspection/debugging. Not part of the primary
request flow (see [Conventions](#conventions)).

**Payload**: none.

**Response** `200 OK`:
```json
{ "xid": 42 }
```

---

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
as on-disk `DiskBTree`s, the vector index as an on-disk IVF-Flat), so a present
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

### `POST /tables/{table}/events`

Opt a table into event capture (M4). From this point on, every
INSERT/UPDATE/DELETE on `table` also durably writes a row to the internal
`__events__` table under the same transaction. Required before
`GET /events/subscribe` or `POST /events/ack` return anything meaningful
for that table.

**Payload**: none.

**Response**: `204 No Content` on success.

---

### `GET /events/subscribe`

Server-Sent Events stream of new events on tables that have event capture
enabled. **This is a server poll loop, not WAL-level push** — the server
calls `poll_events` on an interval and forwards results as SSE frames; see
`sse.rs`'s module doc for the cost model (`N subscribers × poll interval ×
poll_events's own linear-in-table-size cost`, quantified in the M5
benchmark table in `PROGRESS.md`).

**Query parameters**:

| Param | Required | Default | Meaning |
|---|---|---|---|
| `consumer` | yes | — | Durable consumer name; offset is tracked per name |
| `limit` | no | `100` | Max events fetched per poll tick |
| `interval_ms` | no | `500` | Poll interval in milliseconds |

**Response**: `200 OK`, `Content-Type: text/event-stream`, one frame per
new event:
```
id: 17
event: insert
data: {"seq":17,"xid":42,"table_name":"orders","op":"insert","payload":{"id":1,"total":9.99}}

```
Acks are **not** sent over this connection — call `POST /events/ack`
separately (below) once events are durably processed.

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

### `POST /checkpoint`

Trigger `Engine::checkpoint()` manually: flush dirty pages, write a
checkpoint WAL record, persist `next_xid`, truncate the WAL. Operational
route — same auth as everything else in v1 (no admin-only scope).

**Payload**: none.

**Response**: `204 No Content` on success.

---

### `GET /stats` (P6.g)

A `pg_stat_*`-style activity snapshot.

**Response** `200 OK`:
```json
{
  "commits": 42, "aborts": 3, "checkpoints": 1,
  "active_transactions": 0, "wal_bytes": 81920,
  "replication_slots": 1, "max_replication_lag": 128,
  "data_pages": 37, "recent_slow_queries": [{"sql": "...", "micros": 4210}]
}
```

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

**TLS (P6.f):** set `UNIDB_TLS_CERT`/`UNIDB_TLS_KEY` (PEM) to serve HTTPS.

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
```

---

## Error codes

Every error maps through `src/server/error.rs::map_status`. Client-facing
`DbError` variants are listed individually and exhaustively; everything
else (low-level storage/recovery errors a well-formed request should never
trigger) falls into one grouped 500.

| HTTP status | `code` | Triggered by |
|---|---|---|
| 404 | `TABLE_NOT_FOUND` | Referenced table doesn't exist |
| 404 | `COLUMN_NOT_FOUND` | Referenced column doesn't exist |
| 404 | `NOT_FOUND` | Row has no MVCC-visible version (deleted/never existed) |
| 409 | `TABLE_ALREADY_EXISTS` | `CREATE TABLE` on an existing name |
| 409 | `WRITE_CONFLICT` | Concurrent write conflict (lock manager) |
| 409 | `SERIALIZATION_FAILURE` | Snapshot-isolation abort-on-conflict |
| 400 | `SQL_PARSE_ERROR` | Malformed SQL |
| 400 | `SQL_PLAN_ERROR` | SQL that parses but doesn't plan (e.g. bad rewrite) |
| 400 | `SQL_UNSUPPORTED` | Valid SQL outside unidb's supported subset |
| 400 | `NOT_NULL_VIOLATION` | Write left a `NOT NULL`/PK column NULL (M11) |
| 409 | `UNIQUE_VIOLATION` | Write duplicated a `UNIQUE`/`PRIMARY KEY` value (M11) |
| 400 | `CHECK_VIOLATION` | Write failed a `CHECK` constraint (M11) |
| 400 | `FOREIGN_KEY_VIOLATION` | `FOREIGN KEY` references a table that doesn't exist (M11) |
| 400 | `TXN_NOT_ACTIVE` | Operation on a transaction that isn't active |
| 400 | `TXN_ALREADY_FINISHED` | Operation on an already committed/aborted txn |
| 400 | `BAD_PAGE_SIZE` | Invalid page size at open |
| 401 | `UNAUTHORIZED` | Missing/malformed/wrong-signature/expired JWT |
| 503 | `DURABILITY_FAILURE` | An `fsync`/`msync` failed (P1.b, fsyncgate); the engine can no longer guarantee durability and must be restarted (session is poisoned) |
| 500 | `INTERNAL_ERROR` | I/O, checksum, WAL corruption, control-file corruption, catalog corruption, buffer pool exhaustion, or a dead writer thread (`EngineUnavailable`) |

---

## Known limitations

See `PROGRESS.md`'s M5 entry for the full, current list (multi-request
transaction sessions, RLS-over-REST, gRPC, TLS termination, connection
pooling — all explicitly out of scope for v1, not oversights).

---

## Rust attach client

`unidb-attach` (M8) is a Rust crate wrapping every route above in a
one-shot, blocking method call (`AttachClient::execute_sql`, `insert`,
`create_edge`, `edges_from`, `set_column_index`, `enable_events`, etc.) —
no new wire format, just `reqwest::blocking` + the same JSON shapes
documented in this file. It does not expose `vacuum_events`, `vacuum`
(M10 heap GC), `set_rls_policy`, or `flush`, since none of those have a
REST route to call. See the repo root [`README.md`](../README.md#rust-attach-client-unidb-attach-m8)
and [`unidb-attach/src/lib.rs`](../unidb-attach/src/lib.rs).
