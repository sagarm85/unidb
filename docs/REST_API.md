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

JWT bearer auth. One server verifies with exactly one algorithm at a time:

- **HS256** (default) — a shared secret (`UNIDB_JWT_SECRET`).
- **RS256 / ES256** (item 121 A6, optional) — an asymmetric **public** key
  (`UNIDB_JWT_PUBLIC_KEY`, PEM), for verifying tokens minted by an external
  IdP without this server ever holding a shared secret. The algorithm (RSA
  vs EC/P-256) is auto-detected from the key; see
  [`GET /.well-known/jwks.json`](#get-well-knownjwksjson--public-key-discovery-item-121-a6).

Token **verification** is always on. Token **issuance** is an optional
built-in auth service (items 121/122): with a signing key configured —
`UNIDB_JWT_SIGNING_KEY` (item 121 A5, the first-class production path) or
`UNIDB_DEV_LOGIN=1` (pre-A5, still supported) — the server offers real
password login, signup, and refresh-token sessions — see
[`POST /auth/login`](#post-authlogin--password-login),
[`POST /auth/signup`](#post-authsignup--self-service-signup),
[`POST /auth/refresh`](#post-authrefresh--exchange-a-refresh-token), and
[`POST /auth/logout`](#post-authlogout--revoke-a-session). You may still
bring tokens from an external issuer instead; the two modes coexist. Local
issuance is HS256-only and is **disabled outright** when `UNIDB_JWT_PUBLIC_KEY`
is set (an HS256-signed local token could never verify against a configured
asymmetric public key). A user may additionally enroll **TOTP-based MFA**
(item 127) — see
[TOTP-based MFA](#totp-based-multi-factor-authentication-mfa-item-127) — once
enabled, `POST /auth/login` no longer issues a session directly and instead
returns a short-lived challenge that must be redeemed at
`POST /auth/mfa/challenge`. A user may also sign in via **OAuth 2.0 social
login** (item 128, Google/GitHub) — see
[OAuth 2.0 social login](#oauth-20-social-login-item-128-workstream-d1) —
which resolves to the same kind of session through the same issuance path.
Self-service **password reset** and **magic-link (passwordless) login**
(item 138) are also available — see
[Email transport + password reset / magic link](#email-transport--password-reset--magic-link-item-138).

> **Correction (2026-07-31):** earlier versions of this section stated "there
> is no login endpoint, no user database, and no session state." That is no
> longer true — items 121/122 added an argon2id credential store, password
> login/signup, and hash-only refresh-token sessions. Verify-only remains the
> *default posture* when no signing key is configured. **Update (2026-07-31,
> item 121 A5/A6):** issuance now has a first-class production path
> (`UNIDB_JWT_SIGNING_KEY`, independent of `UNIDB_DEV_LOGIN`), and verification
> supports asymmetric RS256/ES256 via `UNIDB_JWT_PUBLIC_KEY` plus a
> `GET /.well-known/jwks.json` discovery route.

```
Authorization: Bearer <jwt signed with UNIDB_JWT_SECRET (HS256), or with the
private key matching UNIDB_JWT_PUBLIC_KEY (RS256/ES256)>
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

## Realtime Broadcast & Presence (item 132)

Supabase-parity gap fill: `GET /events/subscribe` above is
Postgres-Changes-equivalent (WAL-derived row changes). These four routes add
**Broadcast** (ephemeral client↔client pub/sub, not tied to the database) and
**Presence** (who is currently subscribed to a topic, with per-client state).

**Purely in-memory and ephemeral.** Neither broadcast messages nor presence
state touch the WAL, buffer pool, heap, or catalog — nothing is persisted,
and a server restart drops all state (same semantics as Supabase's
Broadcast/Presence). A named **topic** is an opaque caller-chosen string; no
topic needs to be created ahead of time.

**v1 authorization:** every authenticated principal (any JWT that passes
`require_jwt`, the same gate as every other data-plane route) may
publish/subscribe/track on **any** topic — there is no per-topic
allow/deny policy. A `realtime.channels`-style **channel-authorization
policy engine is an explicit, documented follow-up**, not built in v1;
treat every topic as world-readable/writable to any authenticated caller
until that lands.

Transport is SSE (same as `GET /events/subscribe`) — no WebSocket route.

### `POST /realtime/broadcast/publish`

Fan a message out to every current subscriber of `topic`. Best-effort,
at-most-once: a topic with no current subscriber silently drops the
message (the response's `receivers` count reflects that).

**Payload**:
```json
{ "topic": "room:42", "event": "cursor-move", "payload": {"x": 10, "y": 20} }
```

**Response** `200 OK`:
```json
{ "receivers": 2 }
```

---

### `GET /realtime/broadcast/subscribe?topic=<t>`

SSE stream of every message published to `topic` from the moment this
subscription is registered (registration completes **before** the response
headers are sent, so a publish immediately after a successful subscribe is
never lost to a connect race).

**Response**: `200 OK`, `Content-Type: text/event-stream`, one frame per
publish:
```
event: cursor-move
data: {"topic":"room:42","event":"cursor-move","payload":{"x":10,"y":20},"ts":1755000000000}

```
`ts` is milliseconds since the Unix epoch. A subscriber that falls too far
behind the publish rate (channel capacity 256) has older frames silently
dropped for it — documented backpressure onto that one slow subscriber,
never onto the publisher or onto other subscribers.

---

### `GET /realtime/presence/subscribe?topic=<t>`

SSE stream that first emits a `sync` frame (the full current presence map
for `topic`) and then `join`/`leave`/`update` deltas as they happen. **This
connection's own lifetime is its own presence membership contribution** —
see `POST /realtime/presence/track` below for exactly how a tracked key is
tied to it, and what happens on disconnect.

**Response**: `200 OK`, `Content-Type: text/event-stream`:
```
event: sync
data: {"topic":"room:42","event":"sync","payload":{"alice":{"status":"online"}},"ts":1755000000000}

event: join
data: {"topic":"room:42","event":"join","payload":{"key":"bob","state":{"status":"online"}},"ts":1755000000100}

event: leave
data: {"topic":"room:42","event":"leave","payload":{"key":"alice"},"ts":1755000000200}

```
`update` frames use the same shape as `join` (`{"key": ..., "state": ...}`).

---

### `POST /realtime/presence/track`

Associate/update the caller's presence state under `key` on `topic`, and
push a `join` (new key) or `update` (existing key) delta to the topic's
presence subscribers.

**Payload**:
```json
{ "topic": "room:42", "key": "alice", "state": {"status": "online"} }
```

**Response**: `204 No Content`.

**v1 connection-binding model:** the wire format above (matching the spec
exactly) carries no connection id, so a tracked key is attributed to
**every currently-open `GET /realtime/presence/subscribe` connection for
that topic from the same caller (JWT `sub`)** — i.e. "this identity is
present on this topic for as long as at least one of its presence/subscribe
connections stays open." `leave` fires once the *last* such connection
closes. Calling `track` with no live presence/subscribe connection open yet
for that (topic, caller) creates an entry with no holder, which persists
until a matching connection later opens and closes (a documented, bounded
v1 gap — not indefinitely dangerous, still wiped on restart, just not
auto-reaped by a disconnect that never happens).

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

### `PUT /config/group_commit_window_us` (item 101)

**Superuser-gated** (same gate as `PUT /config/slow_query_threshold_ms`).
Updates the WAL group-commit dwell window at runtime without a server restart.

**Request body:**
```json
{ "value": 500 }
```

`value: 0` disables the window (the default — every commit fsyncs
immediately). A positive value (microseconds, e.g. `500`) makes the
flush-lock leader sleep that long before fsyncing, giving concurrent
committers time to coalesce into a single fsync. This trades a small
per-commit latency floor for higher commit throughput under concurrency;
leave it at `0` for single-connection or latency-sensitive workloads.

The window can also be set at engine open via the
`UNIDB_GROUP_COMMIT_WINDOW_US` environment variable (absent or `0` =
disabled, the default).

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
-- Create a role (roles do not carry SUPERUSER; use CREATE USER for that)
CREATE ROLE analyst;

-- Create a user (optionally a superuser)
CREATE USER admin SUPERUSER;

-- Reset an existing user's password (Studio "reset password" action).
-- Superuser-gated exactly like CREATE USER ... PASSWORD; errors if the user
-- doesn't exist. Sets the same argon2id credential set_password()/CREATE
-- USER ... PASSWORD use — no separate credential storage.
ALTER USER admin PASSWORD 'new-password-here';

-- Drop a role or user
DROP ROLE analyst;
DROP USER admin;
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

#### Column-level grants (item 112)

A column list **narrows** a table-level grant to exactly those columns —
holding table-level `SELECT` (the forms above) still implies every column,
including columns added later by `ALTER TABLE ADD COLUMN`; this is
unaffected by column-level grants and needs no migration.

```sql
-- Grant SELECT on only two columns (e.g. hide password_hash from support)
GRANT SELECT (email, name) ON users TO support;

-- Grant UPDATE on only one column
GRANT UPDATE (status) ON tickets TO agent;

-- A single column list applies to every privilege named alongside it
GRANT SELECT, UPDATE (a, b) ON t TO r;

-- Column-scoped REVOKE narrows the existing grant (removes just these
-- columns); a table-level REVOKE (no column list) still clears the
-- privilege entirely, regardless of column scope.
REVOKE SELECT (email) ON users FROM support;
```

Once a grantee's privilege on a table is column-scoped, every reference to a
column of that kind — `SELECT` list, `WHERE`/`JOIN ON`/`GROUP BY`/`HAVING`/
`ORDER BY` predicates (checked as reads), `UPDATE` `SET` targets (checked as
writes) and their right-hand-side expressions (checked as reads), `INSERT`
column lists (writes), and `RETURNING` columns (reads) — is checked against
the granted column set. **`SELECT *` (and `RETURNING *`) requires holding
every column** — a column-scoped grantee who doesn't gets `403
PERMISSION_DENIED` naming the missing column, never a silently NULL-filled or
dropped column. A column referenced only inside an RLS policy predicate
(never in the caller's own SQL) is exempt, matching Postgres — policy
evaluation is not subject to the caller's own column grants.

`information_schema.columns` reflects a column-scoped grantee's `SELECT`
grant: only the granted columns are listed (with `ordinal_position` still
reflecting the column's real position in the table, not a renumbering).
`unidb_catalog.grants` gains a `columns` column: `"ALL"` for a whole-table
grant, else a comma-joined list of exactly the granted columns.

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

#### Token-bound RLS — `auth.uid()`, `auth.jwt()`, and roles (item 122)

Policies can reference the caller's verified token, so a single policy scopes data
per-user and per-tenant without app-side filtering:

```sql
-- The token subject (sub claim). Fails closed to NULL when absent.
CREATE POLICY own_rows ON docs FOR ALL USING (owner = auth.uid());

-- Any verified JWT claim by key. Parenthesise: ->> binds looser than =.
CREATE POLICY tenant_iso ON docs FOR SELECT USING (tenant_id = (auth.jwt() ->> 'tenant'));
```

Both `auth.uid()` and `auth.jwt() ->> 'claim'` are substituted at policy-injection time
(before the Query/QExpr conversion) and **fail closed** — a missing subject or claim
resolves to `NULL` (never `TRUE`), so a policy never widens access.

**Built-in roles.** Every caller resolves to effective roles: `anon` (no verified
subject), `authenticated` (a verified subject) plus the user's granted roles, or
`service_role` (a token whose verified claims carry `"role": "service_role"`).
`anon`/`authenticated`/`service_role` are reserved — `CREATE ROLE`/`DROP ROLE` reject
them. `service_role` **bypasses RLS** like a superuser, but the bypass is written to
`audit.log` (distinct from the implicit-superuser bypass).

**Role-scoped policies.** A policy may target roles with `TO`:

```sql
CREATE POLICY only_auth ON docs FOR SELECT TO authenticated USING (published = true);
```

A policy with **no `TO`** applies to every caller (unchanged behavior). A `TO`-scoped
policy is only applied when the caller's effective roles intersect its target set. If a
table has *only* `TO`-scoped policies and the caller matches none, all rows are denied
(fail closed). Claims/roles come **only** from a verified token — never from the SQL text.

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

#### WITH CHECK on UPDATE policies (item-24 R-a)

When a policy is created with `FOR UPDATE` (or `FOR ALL`) and no explicit `WITH CHECK`,
the `USING` predicate doubles as the write-side check — matching Postgres semantics. An
explicit `WITH CHECK` clause may differ from `USING`:

```sql
-- USING filters which rows alice can target; WITH CHECK validates the NEW row.
CREATE POLICY pos ON scores FOR UPDATE
  USING (val >= 10) WITH CHECK (val >= 10);
```

A write whose new row violates `WITH CHECK` is rejected with a
`new row violates WITH CHECK policy for table "<name>"` error. Superusers and the
embedded API path bypass all write-side checks.

The `with_check_expr` and `enforced` columns in `unidb_catalog.policies` surface these
fields for introspection:

| Column | Meaning |
|--------|---------|
| `with_check_expr` | `NULL` when no explicit `WITH CHECK`; the SQL expression otherwise |
| `enforced` | `false` when no users have been created yet (bootstrap / open mode) |

#### `GET /auth/meta` — discovery endpoint (item 100)

Returns static capability metadata. **Public — no JWT required.** Intended for client
libraries and admin UIs to discover the server's auth configuration before issuing a
login or before prompting for credentials.

```
GET /auth/meta
```

**Response** `200 OK`:
```json
{
  "open_mode": true,
  "privilege_types": ["SELECT", "INSERT", "UPDATE", "DELETE"],
  "policy_operations": ["ALL", "SELECT", "INSERT", "UPDATE", "DELETE"],
  "catalog_tables": ["information_schema.tables", "unidb_catalog.roles", "unidb_catalog.grants", "unidb_catalog.policies"],
  "dev_login_enabled": false,
  "signup_enabled": false
}
```

| Field | Meaning |
|-------|---------|
| `open_mode` | `true` when no users exist — RLS inactive, all callers have full access |
| `dev_login_enabled` | `true` only when server is started with `UNIDB_DEV_LOGIN=1` |
| `signup_enabled` | `true` only when started with `UNIDB_ALLOW_SIGNUP=1` (item 121 A3) |

#### `GET /.well-known/jwks.json` — public key discovery (item 121 A6)

Returns the server's configured asymmetric public key as a [JWK
Set](https://www.rfc-editor.org/rfc/rfc7517), so an external verifier (or a
client SDK) can fetch it instead of hard-coding key material. **Public — no
JWT required.**

```
GET /.well-known/jwks.json
```

**Response** `200 OK` — RSA example (`UNIDB_JWT_PUBLIC_KEY` is an RSA key):
```json
{ "keys": [ { "kty": "RSA", "use": "sig", "alg": "RS256", "n": "<base64url modulus>", "e": "<base64url exponent>" } ] }
```
EC example (`UNIDB_JWT_PUBLIC_KEY` is a P-256 key):
```json
{ "keys": [ { "kty": "EC", "use": "sig", "alg": "ES256", "crv": "P-256", "x": "<base64url>", "y": "<base64url>" } ] }
```
When the server verifies HS256 only (no `UNIDB_JWT_PUBLIC_KEY` configured):
```json
{ "keys": [] }
```
The HS256 shared secret is never published here — there is nothing to publish
for a symmetric key, and this route only ever serializes a public key.

#### Auth rate limiting (item 121 I1)

`POST /auth/login`, `/auth/signup`, `/auth/refresh`, (item 127)
`/auth/mfa/challenge`, and (item 138) `/auth/recover`, `/auth/verify`,
`/auth/magiclink`, `/auth/magiclink/verify` — the routes reachable with no
bearer token at all — are brute-force targets, so all eight sit behind a
shared, in-memory, per-key rate limiter (a hand-rolled fixed window; no
external rate-limit crate). `/auth/mfa/challenge`/the item-138 verify routes
each get their own independent bucket (keyed by IP+path, same as
`/auth/refresh` — their bodies have no `username` field to additionally key
on) — guessing an opaque token or a 6-digit TOTP code is exactly the kind of
brute-force target this limiter exists for. **Read/data routes, `/sql`,
`/metrics`, `/.well-known/jwks.json`, `/auth/meta`, `/auth/logout`, and the
authenticated `/auth/mfa/enroll` · `/verify` · `/disable` routes (already
gated by requiring a valid JWT) are never rate-limited.**

- **Key:** the client's TCP peer IP address (`X-Forwarded-For` is
  deliberately **not** trusted — it is client-supplied and this server has no
  trusted-proxy configuration to validate it; see `src/server/rate_limit.rs`'s
  module doc), plus the route path, plus — when the JSON body carries a
  `username` field (login/signup) — the username itself, so two accounts
  behind the same NAT/proxy IP don't share one bucket. A rejected (401/403)
  attempt counts toward the limit exactly like an accepted one.
- **Config:** `UNIDB_AUTH_RATE_LIMIT` (max attempts per window, default `10`)
  and `UNIDB_AUTH_RATE_WINDOW_SECS` (window length in seconds, default `60`).
  Set `UNIDB_AUTH_RATE_LIMIT=0` to disable rate limiting entirely.
- **Response when exceeded:** `429 Too Many Requests`, body
  `{ "error": "...", "code": "RATE_LIMITED" }`, plus a `Retry-After: <seconds>`
  header (time left in the current window, rounded up to at least 1).

#### CAPTCHA / bot protection (item 131, Workstream I2)

Complements the rate limiter above: instead of throttling by request volume,
`POST /auth/login`/`/auth/signup`/(item 138) `/auth/recover`/`/auth/magiclink`
can each require a verified CAPTCHA token per request (provider-agnostic;
[Cloudflare Turnstile](https://developers.cloudflare.com/turnstile/) ships
as the default). **Disabled by default** — with no config set, every route
behaves exactly as documented below with no `captcha_token` involved at
all.

- **Config:** `UNIDB_CAPTCHA_PROTECT` (default **empty = disabled**) —
  comma-separated list of `login`/`signup`/`recover`/`magiclink` naming
  which routes require a token. `UNIDB_CAPTCHA_PROVIDER` (default
  `turnstile`; also `hcaptcha` / `recaptcha` — all three speak the same
  siteverify wire shape, so only the default verify URL differs).
  `UNIDB_CAPTCHA_SECRET` — the provider's server-side secret key; resolved
  **vault-first** (secret name `captcha.secret`, `unidb-vault set
  captcha.secret`, item 129) and falling back to this env value, exactly
  like OAuth's client secret. `UNIDB_
  CAPTCHA_VERIFY_URL` — override the siteverify endpoint (for pointing a
  deployment at a self-hosted/mocked verifier).
- **Request field:** `POST /auth/login`, `POST /auth/signup`, `POST
  /auth/recover`, and `POST /auth/magiclink` each accept an optional
  `captcha_token` field — the token the client-side CAPTCHA widget
  produced. Required only when the route is named in
  `UNIDB_CAPTCHA_PROTECT`; otherwise ignored (safe to omit).
- **Where it runs:** inside the handler, after the I1 rate limiter and after
  the route's own enable/signing-key gates, but **before any credential
  work** (password verification on login; user creation on signup) — a
  failed CAPTCHA check never touches the password path or creates an
  account.
- **Failure contract:** a required-but-missing (or empty-string) token is
  `400 CAPTCHA_TOKEN_REQUIRED`. An invalid/expired token, a provider
  rejection, or the verifier itself being unreachable/misconfigured are all
  `403 CAPTCHA_FAILED` — deliberately uniform (no oracle on *why*
  verification failed), mirroring `POST /auth/login`'s existing
  `401 INVALID_CREDENTIALS` uniformity. Both are fail-closed: nothing short
  of an explicit `{"success": true}` from the provider lets the request
  through once the route is protected.

#### `POST /auth/login` — password login

> Requires a signing key: `UNIDB_JWT_SIGNING_KEY` (item 121 A5, the
> first-class production path) or `UNIDB_DEV_LOGIN=1` (pre-A5, still
> supported). When neither is configured — or `UNIDB_JWT_PUBLIC_KEY`
> (asymmetric verify mode, item 121 A6) is set instead — the route returns
> the "issuance disabled" error. Rate-limited (item I1, see above) before
> production exposure.

Verifies the supplied password against the user's stored **argon2id** credential
(item 121 A1/A2) and, on success, issues an access token + a refresh token.
The user must have a password set (`CREATE USER … PASSWORD '…'` or signup).

**Security:** unknown user, wrong password, and a user with no credential all
return the **same** `401 INVALID_CREDENTIALS` — a same-cost dummy verify runs on
the miss paths, so there is no user-enumeration or timing oracle. `open_mode`
(no users registered) is unchanged.

**Auth:** None (public).

**Request** `POST /auth/login`:
```json
{ "username": "alice", "password": "correct horse battery staple" }
```

**Response** `200 OK` (no MFA enabled for this user — the pre-item-127 shape,
byte-for-byte unchanged):
```json
{ "token": "<access-jwt>", "access_token": "<access-jwt>", "refresh_token": "<opaque-256bit-hex>", "expires_in": 3600 }
```
> `token` is a deprecated alias for `access_token`, kept for backward compat.
> The `refresh_token` is an opaque high-entropy string (NOT a JWT); the server
> stores only its SHA-256 hash, never the raw token.

**Response** `200 OK` (item 127 — this user has TOTP MFA enabled): **no
session is issued.** Instead:
```json
{ "mfa_required": true, "challenge": "<opaque-hex>", "expires_in": 300 }
```
Redeem `challenge` plus a live TOTP/recovery code at
[`POST /auth/mfa/challenge`](#totp-based-multi-factor-authentication-mfa-item-127)
to receive the real `{access_token, refresh_token, expires_in}` session. The
two response shapes are unambiguous (`mfa_required` is present in one and
absent, along with every token field, in the other) — a client checks for
`mfa_required` to decide which flow it's in.

**Error responses:** `401 INVALID_CREDENTIALS` (unknown user / wrong password /
no credential — uniform); issuance-disabled error when no signing key is set;
`429 RATE_LIMITED` (item I1, see above) once the per-key attempt limit is hit.

#### `POST /auth/signup` — self-service signup

> **Disabled by default.** Enable with `UNIDB_ALLOW_SIGNUP=1` **and** a configured
> signing key (`UNIDB_JWT_SIGNING_KEY` or `UNIDB_DEV_LOGIN=1`). When disabled the
> route returns `404` (indistinguishable from a non-existent route). Item 121 A3.

Creates a **non-superuser** with an argon2id credential and returns the same
token pair as login. Duplicate usernames are rejected; the user is only created
after the signing-key check, so a disabled issuer never leaves an orphaned account.

**Request** `POST /auth/signup`:
```json
{ "username": "bob", "password": "…" }
```
**Response** `200 OK`: same shape as `POST /auth/login`. **Errors:** `404` when
signup disabled; `409`/error on duplicate username; `429 RATE_LIMITED` (item I1).

#### `POST /auth/refresh` — exchange a refresh token

Verifies a refresh token (hashes it, looks up the session; unknown / expired /
revoked all return a uniform `401 INVALID_REFRESH_TOKEN`) and issues a fresh
access token **and a rotated refresh token** (the old one is revoked). Item 121 A4.

**Request** `POST /auth/refresh`:
```json
{ "refresh_token": "<opaque-hex>" }
```
**Response** `200 OK`: same shape as `POST /auth/login` (new access + new refresh).
**Errors:** `401 INVALID_REFRESH_TOKEN`; `429 RATE_LIMITED` (item I1).

#### `POST /auth/logout` — revoke a session

Revokes the session behind a refresh token. **Idempotent** — unknown, garbage, or
already-revoked tokens all return `204 No Content` (no state disclosure). Item 121 A4.

**Request** `POST /auth/logout`:
```json
{ "refresh_token": "<opaque-hex>" }
```
**Response** `204 No Content`.

#### `DELETE /auth/sessions/{id}` — revoke a specific session

Revokes one refresh-token session by its opaque `session_id` (as surfaced by
`unidb_catalog.sessions`, below — never the raw refresh token or its hash).
Self/superuser gated: a superuser (a token with no `sub`, a named
`SUPERUSER`, or open/bootstrap mode) may revoke any session; a named
non-superuser may only revoke a session they own. **Idempotent and
shape-uniform** — an unknown id and a session that belongs to someone else
both return `204 No Content` without the foreign session being touched, so a
non-superuser caller can never distinguish "no such session" from "that
session isn't yours" (the same posture as `POST /auth/logout`).

```
DELETE /auth/sessions/{id}
Authorization: Bearer <token>
```

**Response** `204 No Content` in every case (success, unknown id, or a
foreign session left untouched).

#### `GET /auth/whoami` — caller identity and privileges (item 100)

Returns the authenticated caller's identity, role memberships, and per-table
privilege grants. Requires a valid JWT.

```
GET /auth/whoami
Authorization: Bearer <token>
```

**Response** `200 OK`:
```json
{
  "user": "alice",
  "is_superuser": false,
  "roles": ["reader"],
  "privileges": [
    { "table": "posts", "ops": ["SELECT"] }
  ],
  "open_mode": false,
  "mfa_enabled": false
}
```

| Field | Meaning |
|-------|---------|
| `user` | JWT `sub` claim; `null` in open mode with no user identity |
| `is_superuser` | `true` when the user was created with `SUPERUSER` |
| `open_mode` | Mirrors `GET /auth/meta` — no users registered yet |
| `mfa_enabled` | Item 127: `true` when the caller has TOTP MFA enabled. Never the secret or recovery codes — just the boolean. `false` for the implicit superuser (token with no `sub`). |

### TOTP-based multi-factor authentication (MFA) (item 127)

A user can enroll a TOTP authenticator (Google Authenticator, 1Password,
Authy, …) and, once enrolled, login requires a valid 6-digit code as a
second factor. Entirely self-contained — no external provider, no secrets
beyond what this server generates and stores itself.

**Crypto:** HMAC-SHA1 over the 30 s time counter (RFC 6238 / Google
Authenticator defaults), dynamic truncation, 6 digits. ±1 step (±30 s) of
clock-skew tolerance. Codes are compared in constant time. **Replay
protection** is a forward-only ratchet: the most recently *successfully
verified* TOTP step is remembered per user, and any step at or before it is
rejected outright — the exact code just consumed (or any earlier-window
code) can never authenticate a second time, even replayed instantly. This
also means at most 3 fresh TOTP verifications are possible within one
static 30 s window (the ±1-step tolerance is only 3 steps wide); one-time
**recovery codes** are a separate credential space unaffected by this budget.

**Storage:** the base32 TOTP secret and recovery-code SHA-256 hashes live on
the user in `roles.json` (the same control-plane store as credentials/
sessions), covered by the same manual `Debug` redaction — never logged,
`Debug`-printed, returned by `whoami`, or exposed in any catalog view.
Recovery-code plaintext is shown to the client **exactly once**, at
confirmation time; only the hash is ever persisted.

#### `POST /auth/mfa/enroll` — start enrollment

**Auth:** JWT required (named user — not the implicit superuser).

Generates a fresh, per-user 160-bit TOTP secret and stores it **pending**
— MFA is **not** enabled yet. Re-enrolling while already enabled is
rejected (disable first); re-enrolling while still pending simply replaces
the secret.

```
POST /auth/mfa/enroll
Authorization: Bearer <token>
```

**Response** `200 OK`:
```json
{
  "secret": "JBSWY3DPEHPK3PXP",
  "otpauth_url": "otpauth://totp/unidb:alice?secret=JBSWY3DPEHPK3PXP&issuer=unidb"
}
```
Render `otpauth_url` as a QR code, or let the user type `secret` in
manually. **Errors:** `400` if the caller has no named identity (implicit
superuser); `400 AUTHZ_ERROR` if MFA is already enabled.

#### `POST /auth/mfa/verify` — confirm enrollment

**Auth:** JWT required (same named user that enrolled).

Confirms a pending enrollment with a live 6-digit code. On success, MFA
flips to **enabled** and a batch of 8 one-time recovery codes is returned.

**Request** `POST /auth/mfa/verify`:
```json
{ "code": "123456" }
```
**Response** `200 OK`:
```json
{
  "enabled": true,
  "recovery_codes": ["a1b2c3-d4e5f6", "..."]
}
```
**Errors:** `401 MFA_INVALID_CODE` (wrong/malformed/replayed code — uniform,
no oracle); `400 AUTHZ_ERROR` if there is no pending enrollment, or MFA is
already enabled.

#### `POST /auth/mfa/challenge` — redeem an MFA login challenge

**Auth:** None (public — the challenge token itself is the pre-session
credential, same posture as `POST /auth/refresh`'s refresh token).
Rate-limited (item I1, see above).

Redeems the `challenge` issued by `POST /auth/login` (see that route's docs
above) plus a live TOTP code **or** a one-time recovery code for a real
session — reuses the exact same session-issuance path every other login
does, so the minted session is indistinguishable from a non-MFA login's.

**Request** `POST /auth/mfa/challenge`:
```json
{ "challenge": "<opaque-hex-from-login>", "code": "123456" }
```
**Response** `200 OK`: same shape as `POST /auth/login`'s non-MFA response
(`{token, access_token, refresh_token, expires_in}`).

**Errors:** `401 MFA_CHALLENGE_INVALID` — uniform for every failure case:
unknown/garbage challenge, expired challenge (5 minute TTL), already-used
(single-use) challenge, wrong/replayed code, or MFA having been disabled
between issuing the challenge and redeeming it. `429 RATE_LIMITED` (item I1).

#### `POST /auth/mfa/disable` — turn MFA off

**Auth:** JWT required. Acts on the caller's own account.

Requires a currently-valid TOTP or recovery `code` in the body, **unless**
the caller is a superuser (the same effective-superuser rule as every other
admin gate) — in which case no code is needed (emergency account-recovery
path for a user who lost both their authenticator and recovery codes).

**Request** `POST /auth/mfa/disable`:
```json
{ "code": "123456" }
```
**Response** `204 No Content`.

**Errors:** `400 MFA_CODE_REQUIRED` (non-superuser, no code supplied);
`401 MFA_INVALID_CODE` (present-but-wrong code — uniform, no oracle on
*why*); `400 AUTHZ_ERROR` if MFA isn't currently enabled.

### OAuth 2.0 social login (item 128, Workstream D1)

A user can "Sign in with Google/GitHub": the app redirects to the provider,
the provider redirects back with a code, unidb exchanges it for a provider
access token, links/creates a unidb identity, and issues a normal unidb
session — via the exact same [`issue_token_pair` helper](#post-authlogin--password-login)
every other login path (password, signup, MFA challenge) uses. Standard
**Authorization Code + PKCE** (RFC 7636), provider-agnostic: Google and
GitHub are the two recognized provider names, but nothing in the flow itself
is provider-specific beyond three URLs and a scope string.

**Config** (per provider — `<PROVIDER>` is `GOOGLE` or `GITHUB`):
- `UNIDB_OAUTH_<PROVIDER>_CLIENT_ID` / `_CLIENT_SECRET` / `_REDIRECT_URI`
  (all three **required**) — a provider missing any of these is simply not
  configured: both its routes return `404`, indistinguishable from a
  non-existent route, same posture as `UNIDB_DEV_LOGIN`/`UNIDB_ALLOW_SIGNUP`
  being unset. **The server works safely with zero providers configured.**
- `UNIDB_OAUTH_<PROVIDER>_AUTHORIZE_URL` / `_TOKEN_URL` / `_USERINFO_URL` /
  `_SCOPE` (optional) — override the real Google/GitHub endpoint defaults,
  mainly for pointing a provider at a local mock in tests.
- OAuth login still needs a signing key configured (`UNIDB_JWT_SIGNING_KEY`
  or `UNIDB_DEV_LOGIN=1`) to hand back a session — same requirement as
  signup/refresh.

**Secret handling (item 129, Workstream I3 — shipped):** the *effective*
client secret is resolved by `oauth::resolve_client_secret` at
token-exchange time: it checks the [secrets vault](#secrets-vault-item-129-workstream-i3)
first, under the name `oauth.<provider>.client_secret`, and falls back to
the `UNIDB_OAUTH_<PROVIDER>_CLIENT_SECRET` env value (via
`OAuthProviderConfig::client_secret()` / `ClientSecret::expose()` in
`src/server/oauth.rs`) when no vault secret is stored. `_CLIENT_SECRET` is
therefore now **optional** in the environment — a provider can be
configured vault-only (`_CLIENT_ID`/`_REDIRECT_URI` in env, the secret
stored via `unidb-vault set oauth.<provider>.client_secret`). Whichever
source is used, the value is never logged, never `Debug`-printed, never
returned in any response; a vault secret that WAS stored but can't be
decrypted (vault disabled, wrong/rotated key, tampered blob) is a hard
`502 OAUTH_PROVIDER_UNAVAILABLE` at callback time — never a silent
plaintext fallback.

**Identity store:** `(provider, provider_user_id) -> unidb username` is
persisted in `roles.json` (the same control-plane store as
credentials/sessions/MFA), serde-default so an existing `roles.json` loads
unchanged. No secret material lives there (just ids and the resolved
username) — it is not redacted from the store's internal `Debug`, unlike
credentials/sessions/MFA state. No provider access token is ever persisted
(only used in-flight to fetch userinfo, then discarded).

**Identity linking rule (create, never auto-link by email):** a returning
identity always resolves via the `(provider, provider_user_id)` map, never
by matching a claimed `email` against an existing local account.
Auto-linking by email would let anyone who controls *any* OAuth identity
sharing an email string silently take over an existing password-protected
unidb account — a real account-takeover surface. First login for a new
identity creates a fresh **non-superuser, no-password** account named
`oauth_<provider>_<provider_user_id>`. A future D5 (email flows) may offer
an explicit, user-initiated "link this OAuth identity to my account" action
once verified email exists.

#### `GET /auth/oauth/{provider}/authorize` — start the OAuth flow

**Auth:** None (public — this route *establishes* identity).

`{provider}` is `google` or `github` (or any name configured via
`OAuthConfig::from_providers`). Mints a fresh CSRF `state` token and a PKCE
`code_verifier`, persists them server-side (single-use, 10-minute TTL — the
same "hash-only, short-lived, single-use" posture as the MFA
challenge/refresh-token session stores), derives the PKCE `code_challenge`
(`S256` — base64url-no-pad of the SHA-256 digest of the verifier), and
redirects:

```
GET /auth/oauth/google/authorize

302 Found
Location: https://accounts.google.com/o/oauth2/v2/auth?client_id=...&redirect_uri=...&scope=...&state=...&code_challenge=...&code_challenge_method=S256&response_type=code
```

The PKCE `code_verifier` itself is never sent to the client — only the
derived `code_challenge` is, per RFC 7636. **Errors:** `404 NOT_FOUND` for
an unconfigured provider.

#### `GET /auth/oauth/{provider}/callback` — finish the OAuth flow

**Auth:** None (public — the provider's redirect *is* the credential, same
posture as `POST /auth/refresh`'s refresh token).

```
GET /auth/oauth/google/callback?code=<from-provider>&state=<from-authorize>
```

1. If the provider reports `?error=...` (user denied consent, etc.), fails
   immediately with `400 OAUTH_PROVIDER_DENIED`.
2. Validates + single-use-consumes `state` — unknown, expired, replayed, or
   issued-for-a-different-provider all return the identical `401
   OAUTH_STATE_INVALID` (no oracle on *why*).
3. Exchanges `code` for a provider access token (server-to-server `POST`
   with the PKCE `code_verifier`) — an unreachable/erroring provider maps to
   `502 OAUTH_PROVIDER_UNAVAILABLE`; an explicit provider rejection (bad
   code/verifier/redirect_uri) maps to `401 OAUTH_TOKEN_EXCHANGE_FAILED`.
4. Fetches the provider's userinfo (id + email) with that access token —
   same `502`/`401` split on failure.
5. Resolves `(provider, provider_user_id)` to a unidb user (see "Identity
   linking rule" above).
6. Issues a real session via the same path every other login uses.

**Response** `200 OK` — same shape as `POST /auth/login`'s non-MFA response:
```json
{ "token": "...", "access_token": "...", "refresh_token": "...", "expires_in": 3600 }
```

**Errors:** `404 NOT_FOUND` (unconfigured provider); `400
OAUTH_PROVIDER_DENIED` (provider-reported `error`); `400 OAUTH_MISSING_CODE`
(no `code` param and no `error` either); `401 OAUTH_STATE_INVALID`; `401
OAUTH_TOKEN_EXCHANGE_FAILED`; `502 OAUTH_PROVIDER_UNAVAILABLE`.

**No rate limiting on these two routes** (deliberately — see
`router.rs`'s comment): unlike login/signup/refresh, neither accepts a
guessable credential — `authorize` takes no input at all, and
`callback`'s `state`/`code` are both high-entropy, server-validated,
single-use tokens with no meaningful brute-force surface for a
fixed-window IP limiter to protect.

### Email transport + password reset / magic link (item 138)

unidb can send outbound email for two self-service auth flows: password
reset and magic-link (passwordless) login. **No `users.email` column
exists yet** — `email` in the requests below is looked up **directly as a
username**, i.e. this milestone assumes an account's username *is* its
email address (a supported convention today via `POST /auth/signup`). A
real `users.email` column is tracked as a fast follow-up in
`docs/backlog/137_supabase_parity_free_roadmap.md`; the request shape
(`{"email": "..."}`) will not need to change when it lands.

**Transport config** — `UNIDB_EMAIL_TRANSPORT` (default `log`):
- `log` (default) — writes the fully-rendered email to a dev-inbox file
  (`UNIDB_EMAIL_DEV_FILE`, default `<UNIDB_DATA_DIR>/email-dev-inbox.jsonl`,
  one JSON line per send: `{to, from, subject, text_body, html_body}`) and
  to `tracing`, instead of actually sending — no mail server needed for
  local dev/tests (mirrors Supabase's Inbucket/Mailpit).
- `smtp` — real delivery via `lettre`. Requires `UNIDB_SMTP_HOST` and
  `UNIDB_SMTP_FROM` (the server refuses to start otherwise, with a clear
  error naming which is missing); also reads `UNIDB_SMTP_PORT` (default
  `587`), `UNIDB_SMTP_USERNAME`, and `UNIDB_SMTP_TLS_MODE` (`tls` — the
  default, implicit TLS/SMTPS — | `starttls` | `none`, only for a trusted
  local relay). The password is resolved **vault-first**: a vault secret
  named `smtp.password` (`unidb-vault set smtp.password`) wins over
  `UNIDB_SMTP_PASSWORD` — the exact same order/fail-closed contract as
  OAuth's client-secret resolution above.
- `UNIDB_EMAIL_SITE_URL` (default `http://localhost:8080`) — the base URL
  used to build the `{{link}}` in both templates:
  `{site_url}/auth/verify?token=...` (recovery) /
  `{site_url}/auth/magiclink/verify?token=...` (magic link).

**Templates** — built-in defaults for `"recovery"` and `"magiclink"`
(subject + text + HTML body), each with `{{link}}`/`{{user}}`/
`{{site_url}}`/`{{code}}` substitution; substituted values are HTML-escaped
in the HTML body. Override either (or both) via `UNIDB_EMAIL_TEMPLATES_DIR`
— a directory containing `<name>.txt` (first line = subject, the rest =
the text body) and an optional `<name>.html`.

**No account enumeration:** both `POST /auth/recover` and `POST
/auth/magiclink` **always return `200`** — a registered vs. unregistered
`email` is byte-identical in status, body, and (best-effort) timing. A
token-mint or email-send failure for a *registered* account is logged
server-side (`tracing::warn!`) but still returns `200` — delivery failures
must not become a distinguishable oracle either. Both routes are
rate-limited (item I1) and CAPTCHA-eligible (item I2, endpoint names
`recover`/`magiclink`) exactly like `POST /auth/login`/`/auth/signup`.

**Tokens:** single-use, hash-only persisted (SHA-256, same posture as every
other token in this server — refresh tokens, MFA challenges, OAuth state).
Recovery tokens are valid 1 hour; magic-link tokens 15 minutes (the same
narrow window as an MFA login challenge, since redeeming one mints a full
session with no second factor).

#### `POST /auth/recover` — request a password-reset email

**Auth:** None (public).

**Request:**
```json
{ "email": "alice@example.com" }
```
**Response** `200 OK` (always, regardless of whether `email` is known):
```json
{ "ok": true }
```

#### `POST /auth/verify` — redeem a recovery token for a new password

**Auth:** None (public — the token itself is the pre-session credential).

Validates + single-use-consumes `token`, sets the new argon2id credential
via the same path `ALTER USER ... PASSWORD` uses, and revokes every
existing session for that user (a session obtained with the old password
stops working immediately).

**Request:**
```json
{ "token": "<opaque-hex-from-email>", "new_password": "a-new-password" }
```
**Response** `200 OK`: `{ "ok": true }`.

**Errors:** `401 RECOVERY_TOKEN_INVALID` — uniform for unknown/expired/
already-used token (no oracle on *why*).

#### `POST /auth/magiclink` — request a magic sign-in link email

**Auth:** None (public). Same shape/contract as `POST /auth/recover` above.

**Request:** `{ "email": "alice@example.com" }`. **Response** `200 OK`
(always): `{ "ok": true }`.

#### `POST /auth/magiclink/verify` — redeem a magic-link token for a session

**Auth:** None (public — the token itself is the pre-session credential,
same posture as `POST /auth/mfa/challenge`).

**Request:**
```json
{ "token": "<opaque-hex-from-email>" }
```
**Response** `200 OK` — same shape as `POST /auth/login`'s non-MFA
response: `{ "token": "...", "access_token": "...", "refresh_token": "...", "expires_in": 3600 }`.

**Errors:** `401 MAGICLINK_TOKEN_INVALID` — uniform for unknown/expired/
already-used token.

### Secrets vault (item 129, Workstream I3)

Config secrets — OAuth client secrets and the SMTP password (above) — can
be stored **encrypted at rest** in `roles.json` instead of sitting in
plaintext env vars. This is not an HTTP feature (no new routes): it's a CLI
+ an internal resolution order that the OAuth and email flows already use.

**Master key — `UNIDB_MASTER_KEY`:** a 32-byte AES-256 key, encoded as
either base64 (standard alphabet, padded or unpadded — 44 or 43 characters)
or hex (64 hex digits). Generate one with `openssl rand -base64 32` (or
`-hex 32`). Three states:
- **Unset** — the vault is **disabled**: a startup `warn!` is logged, and
  every secret-backed config value falls back to its plaintext env var
  exactly as before this item shipped. Never a hard crash. A secret that
  was genuinely stored while the vault WAS enabled cannot be read back
  without the original key.
- **Set but malformed** (wrong length, not valid base64/hex) — the server
  (and `unidb-vault`) refuse to start, with a clear error naming the exact
  problem — same posture as a missing `UNIDB_JWT_SECRET`.
- **Set and valid** — the vault is enabled. Encryption is AES-256-GCM with
  a fresh random 96-bit nonce per secret; decryption verifies the GCM
  authentication tag, so a wrong/rotated key or a tampered stored blob both
  fail closed (an error, never a silent plaintext fallback).

**`unidb-vault` CLI** (`cargo run --bin unidb-vault`, or the built binary —
not `server`-feature-gated, same as `unidb-migrate`). Config:
`UNIDB_DATA_DIR` (default `/tmp/unidb`), `UNIDB_PAGE_SIZE`,
`UNIDB_MASTER_KEY` (required for `set`).

```text
unidb-vault set <name>     Encrypt+store a secret. Reads the plaintext from
                            stdin if piped/redirected, otherwise from
                            UNIDB_VAULT_SECRET_VALUE — NEVER a CLI argument
                            (shell history / `ps`). Prints only
                            "<name>: stored".
unidb-vault has <name>     Print "stored" / "not stored". Never decrypts.
unidb-vault list           List stored secret names, one per line.
```

```text
# store an OAuth client secret encrypted at rest
echo -n 'real-google-client-secret' | UNIDB_MASTER_KEY=<key> \
  unidb-vault set oauth.google.client_secret
```

**OAuth resolution order** (`src/server/oauth.rs::resolve_client_secret`,
called from `GET /auth/oauth/{provider}/callback` at token-exchange time):
1. Vault secret named `oauth.<provider>.client_secret` — if stored, its
   decrypted value is used. A stored-but-undecryptable secret (vault
   disabled, wrong key, tampered blob) is a hard `502
   OAUTH_PROVIDER_UNAVAILABLE` — never a fallback to step 2.
2. Otherwise, the `UNIDB_OAUTH_<PROVIDER>_CLIENT_SECRET` env value (now
   optional at provider-config time — a provider needs only `_CLIENT_ID`/
   `_REDIRECT_URI` to be considered configured, so it can be vault-only).
3. If neither is set, the callback fails with `502
   OAUTH_PROVIDER_UNAVAILABLE` naming the missing secret.

With no `UNIDB_MASTER_KEY` and no secret ever stored, resolution always
takes step 2 — OAuth behaves exactly as it did before item 129.

See `docs/backlog/129_secrets_vault.md` for the full design note and
`src/vault.rs`'s module doc for the crypto detail.

#### Catalog virtual relations

The current roles, grants, policies, role memberships, users, and sessions are queryable as
virtual relations:

```sql
SELECT * FROM unidb_catalog.roles;
SELECT * FROM unidb_catalog.grants;
SELECT * FROM unidb_catalog.policies;
SELECT * FROM unidb_catalog.role_members;
SELECT * FROM unidb_catalog.users;
SELECT * FROM unidb_catalog.sessions;
```

These are read-only virtual tables synthesized by the executor from the in-memory `RoleStore`
(which is itself loaded from the persisted catalog at engine open). They do not correspond to
heap pages on disk. Like every `unidb_catalog.*` relation, each requires its own `GRANT SELECT`
for a non-superuser caller (item-24 Z5 — unchanged by any of the below); `information_schema.*`
is the one that needs no grant of its own (item 111).

The `policies` relation includes `with_check_expr` (the explicit write-side predicate, or
`NULL`), `enforced` (`false` in bootstrap/open mode before the first user is created), and
`target_roles`: the `CREATE POLICY … TO <role,...>` role scope (item 122, B4), rendered as a
comma-joined, alphabetically-sorted list of role names (e.g. `"ops,support"`), or `*` when the
policy has no `TO` clause (applies to every caller — the pre-B4, back-compat default). This
column is appended after `enforced`, so existing column-position assumptions are unaffected.

The `sessions` relation (Studio "active sessions" panel) lists refresh-token sessions —
`session_id, username, created_at, expires_at, revoked` — **never** the raw refresh token or its
SHA-256 hash. `session_id` is an independently-random opaque id (a separate 128-bit CSPRNG draw
from the token itself, not a hash prefix or any other derivation of it), stable for the life of
the session and usable with `DELETE /auth/sessions/{id}` above. Visibility mirrors item 111: a
superuser (or open/bootstrap mode) sees every session; a named non-superuser sees only their own.
A session record is retained (with `revoked = true`) after `POST /auth/refresh` rotates it or
`DELETE /auth/sessions/{id}` revokes it — the relation is a full history of a caller's sessions,
not just the currently-live one.

#### Open-mode compatibility

With no roles registered, the server runs in **open mode**: all validly-authenticated users
have full access to all tables. Open mode is the default and maintains full backward
compatibility with deployments that do not use per-user authorization. Once any role or grant
is registered, the engine enforces privileges on all subsequent requests.

The `enforced` column in `unidb_catalog.policies` and the `open_mode` field in
`GET /auth/meta` and `GET /auth/whoami` surface this state for client observability.
The engine also emits a startup `WARN` log when RLS policies exist but no users have
been created (bootstrap signal — policies are defined but currently inactive).

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
| 400 | `INVALID_FILTER` / `INVALID_QUERY_PARAM` / `INVALID_SELECT` | Malformed `/rest/v1` filter, `limit`/`offset`, or `select=` syntax (item 123 C1/C2) |
| 400 | `MULTIPLE_IN_FILTERS` | More than one `in.(...)` filter on a `/rest/v1` `PATCH`/`DELETE` (item 123 C1) |
| 400 | `UNKNOWN_RELATIONSHIP` | `/rest/v1` embed name matches no FK relationship (item 123 C2) |
| 400 | `AMBIGUOUS_RELATIONSHIP` | `/rest/v1` embed name matches more than one FK relationship (item 123 C2) |
| 400 | `UNKNOWN_EMBED_PARAM` | `/rest/v1` dotted param (`<embed>.<col>=…`) whose prefix names no embed in `select=` (item 136) |
| 400 | `OAUTH_PROVIDER_DENIED` | Provider returned `?error=...` on the OAuth callback (item 128) |
| 400 | `OAUTH_MISSING_CODE` | OAuth callback missing both `code` and `error` (item 128) |
| 401 | `OAUTH_STATE_INVALID` | Unknown/expired/replayed/wrong-provider OAuth `state` (item 128) |
| 401 | `OAUTH_TOKEN_EXCHANGE_FAILED` | Provider explicitly rejected the code/PKCE exchange (item 128) |
| 502 | `OAUTH_PROVIDER_UNAVAILABLE` | OAuth provider unreachable, erroring, or unparseable (item 128) |
| 401 | `RECOVERY_TOKEN_INVALID` | Unknown/expired/already-used password-recovery token (item 138) |
| 401 | `MAGICLINK_TOKEN_INVALID` | Unknown/expired/already-used magic-link login token (item 138) |
| 401 | `UNAUTHORIZED` | Missing/malformed/wrong-signature/expired JWT |
| 503 | `DURABILITY_FAILURE` | An `fsync`/`msync` failed (P1.b, fsyncgate); the engine can no longer guarantee durability and must be restarted (session is poisoned) |
| 500 | `INTERNAL_ERROR` | I/O, checksum, WAL corruption, control-file corruption, catalog corruption, buffer pool exhaustion, or an unavailable engine (`EngineUnavailable`) |

**`POST /graphql` is the one exception to this table:** per the GraphQL-over-HTTP
convention it always returns `200`, even on a denied field or a malformed
query — the same `CODE: message` strings above appear as a `"<CODE>:
<message>"` prefix inside the response body's `errors[].message`, not as an
HTTP status. See the C4 section below.

---

---

## Auto-generated data API (`/rest/v1`, item 123)

> **Docs correction:** this section was missing entirely until the C2 pass —
> `/rest/v1` (C1: resource routes + filter operators) had shipped without a
> doc update. Documented here in full, not just the new C2 embedding syntax,
> per CLAUDE.md §9's "no design decision left claiming *not started*" rule.

A PostgREST-style, schema-derived REST API layered over the same enforced
path `POST /sql` uses — every request is translated to a **parameterized SQL
statement** (never string-concatenated) and executed through
`execute_sql_params_as_principal` under the caller's JWT, so RLS and
table/column grants apply exactly as they do for `/sql`. Source of truth:
`src/server/rest_resource.rs`.

### C1 — Resource routes + filter operators

```
GET    /rest/v1/{table}?select=...&<col>=<op>.<value>&order=...&limit=&offset=
POST   /rest/v1/{table}          -- JSON object or array of objects -> INSERT
PATCH  /rest/v1/{table}?<filters> -- JSON object of assignments -> UPDATE
DELETE /rest/v1/{table}?<filters> -- DELETE
```

- **`select=col,col,...`** — column projection; omit for all (non-dropped)
  columns.
- **Filter operators** (`<col>=<op>.<value>`): `eq`, `neq`, `gt`, `gte`,
  `lt`, `lte`, `like`, `ilike`, `in.(v1,v2,...)`, `is.null` / `is.true` /
  `is.false`. Every value is a `$n` bind parameter — never SQL text.
  `id=eq.7&id=gte.3` (repeated keys) ANDs together, same as `&`-separated
  distinct columns.
- **`order=col.asc,col2.desc`** — no suffix defaults to ascending.
- **`limit=`/`offset=`** — plain non-negative integers (not bindable in this
  engine's grammar; validated as clean ASCII digits before ever reaching SQL
  text).
- Unknown table -> `404 TABLE_NOT_FOUND`; unknown column -> `404
  COLUMN_NOT_FOUND`; unsupported operator -> `400 INVALID_FILTER`. Internal
  `__…__` tables (events/edges/lobs/consumers) are hidden as 404, same as
  `GET /tables`.
- `PATCH`/`DELETE` with an `in.(...)` filter is expanded server-side into one
  statement per value (this engine's `UPDATE`/`DELETE` grammar has no native
  `IN`), all inside one transaction with their counts summed; at most one
  `in.(...)` filter per request (`400 MULTIPLE_IN_FILTERS` otherwise). `GET`
  uses native SQL `IN (...)` and has no such limit.

### Response controls — `Prefer` header (item 139)

PostgREST-style response-shape controls via the `Prefer` request header —
pure REST-layer response shaping, no SQL-engine change. Parsed
case-insensitively; the value may be a comma-separated list
(`Prefer: return=representation, count=exact`), and the header may itself be
repeated. **Unrecognized preferences are ignored, never an error** (e.g.
`count=planned`/`count=estimated` — not supported, no planner row estimate
exists to expose — and `resolution=merge-duplicates`/upsert, explicitly out
of scope, see `docs/backlog/139_rest_count_prefer.md`). When at least one
preference was recognized, the response echoes it back verbatim in a
`Preference-Applied` header (comma-joined if more than one).

- **`Prefer: count=exact` on `GET /rest/v1/{table}`** — after the normal
  (possibly `limit`/`offset`-paginated) `SELECT`, runs a **second**
  `SELECT COUNT(*) FROM {table} [WHERE <same filters, same $n binds>]`
  through the identical enforced path (`run_stmt` ->
  `execute_sql_params_as_principal`) as the main query, so **RLS and grants
  apply to the count** — a caller who can see 3 of 10 rows gets a count of 3,
  never 10. The result is reported as a `Content-Range` response header:
  `Content-Range: <from>-<to>/<total>`, where `<from>-<to>` is the returned
  row window (`offset..offset+returned-1`), or `*` when the query returned
  zero rows. Body is unchanged either way.
  - **Without the header, there is no extra query and no `Content-Range`
    header at all** — byte-identical to a plain `GET`.
  ```
  GET /rest/v1/items?limit=2
  Prefer: count=exact
  -> 200, body = first 2 rows, Content-Range: 0-1/5
  ```
- **`Prefer: return=representation` on `POST`/`PATCH`/`DELETE`** — the
  generated statement gains a `RETURNING *` clause (same mechanism
  `graphql.rs`'s `insert_/update_/delete_<t>` mutations already use) and the
  response body becomes the affected rows, in the same
  `{"type":"rows","columns":[...],"rows":[[...]]}` shape `/sql`'s own
  `RETURNING` produces. For `PATCH`/`DELETE` with an `in.(...)` filter (which
  server-side-expands into one statement per value, see C1 above), every
  expanded statement's `RETURNING` rows are merged into one result — the
  caller issued one request, so gets back one combined row set, not N.
- **`Prefer: return=minimal` on `POST`/`PATCH`/`DELETE`** — returns an empty
  body: `201 Created` for `POST`, `204 No Content` for `PATCH`/`DELETE`. The
  mutation still runs exactly as it would with no `Prefer` header; only the
  response shape changes.
- **No `Prefer` header (default) — unchanged from before item 139:**
  - `GET` returns the row body with no `Content-Range` header (as above).
  - `POST` returns `200` with `{"type":"inserted","count":N}`.
  - `PATCH` returns `200` with `{"type":"updated","count":N}`.
  - `DELETE` returns `200` with `{"type":"deleted","count":N}`.

  (Note this default shape — a plain count body at `200`, not PostgREST's own
  default of `201`/`204` with a `Location` header — is this API's pre-existing
  behavior, kept as-is per item 139's explicit non-goal of changing it.)

### C2 — Embedded resource expansion (`GET` only, item 123 C2)

`select=` accepts embed entries of the form `name(col,col,...)` alongside
plain columns — PostgREST-style resource embedding, derived purely from the
catalog's foreign-key metadata (never per-table logic):

```
GET /rest/v1/orders?select=id,total,customer(id,name)
-> [{"id":1,"total":10,"customer":{"id":7,"name":"acme"}}, ...]

GET /rest/v1/customers?select=id,name,orders(id,total)
-> [{"id":7,"name":"acme","orders":[{"id":1,"total":10}, ...]}, ...]
```

(Responses are shown here PostgREST-style for readability; the actual wire
shape follows this API's existing tabular convention —
`{"type":"rows","columns":[...],"rows":[[...]]}` — with the embed's column
holding a nested JSON object/array instead of a scalar.)

- **Forward (many-to-one), MUST:** `name` matches one of the base table's own
  FK columns — the referenced table's name, the FK column's own name, or the
  FK column with a trailing `_id` stripped (`customer_id` -> `customer`).
  Embeds a single object, or `null` when the FK value is `NULL` or the
  referenced row isn't visible to the caller.
- **Reverse (one-to-many), SHOULD — shipped:** `name` matches another
  table's name, where that table carries an FK column targeting the base
  table. Embeds an array (empty when there are no matching children).
- **`name(*)` or `name()`** embeds every column of the related resource;
  `name(col,...)` embeds only the listed (catalog-validated) columns.
- **Ambiguity is `400 AMBIGUOUS_RELATIONSHIP`**, not a silent first match —
  e.g. two FK columns on the same table both targeting the same referenced
  table under the same derived alias. An unresolvable name is `400
  UNKNOWN_RELATIONSHIP`. Malformed `select=` syntax (unbalanced parens) is
  `400 INVALID_SELECT`.
- **Enforcement:** the embedded table is fetched via a *second*
  parameterized query through the identical enforced path as the base query
  — same table/column-grant pre-check, same RLS, same caller principal. A
  restricted caller sees only the embedded rows/columns they could `SELECT`
  directly; a row hidden by RLS nests as `null`/is omitted from the array
  rather than leaking, and requesting an embedded column the caller isn't
  granted denies the **whole request** (`403 PERMISSION_DENIED`), matching a
  direct `/rest/v1` request for that column.
- Base-table filters/order/limit/offset apply only to the base table.
- **Filtering / ordering / paginating an embedded resource (item 136)** —
  dotted per-embed params, PostgREST-style, where the prefix names an embed
  present in `select=`:
  - **Filter:** `<embed>.<col>=<op>.<val>` — same operator grammar as base
    filters (`eq/neq/gt/gte/lt/lte/like/ilike/in/is`), AND-combined with the
    embed's join.
  - **Order:** `<embed>.order=<col>.<asc|desc>[,<col2>...]` — same grammar as
    the base `order`.
  - **Pagination:** `<embed>.limit=<n>` / `<embed>.offset=<n>`, applied
    **per parent row** (lateral semantics — each parent's embedded array is
    sliced independently, *not* a single global cap across all parents).
  ```
  GET /rest/v1/customers?select=id,orders(id,total)&orders.total=gt.100&orders.order=total.desc&orders.limit=3
  -> each customer's `orders` = only their orders with total>100, newest-value
     first, at most 3 per customer.
  ```
  Enforcement is inherited: the embed's filter/order columns run through the
  same second parameterized query, so filtering/ordering on a column the caller
  isn't granted denies the request exactly as a direct embed of that column
  would (values are bind params, names catalog-validated). A dotted param whose
  prefix doesn't name an embed in `select=` is `400 UNKNOWN_EMBED_PARAM`.
- Composite (multi-column) FKs are out of scope for v1 embedding — single-
  column FKs only (column-level `REFERENCES` and single-column table-level
  `FOREIGN KEY`).

### C3 — OpenAPI document

```
GET /rest/v1
GET /rest/v1/
```

Returns a minimal OpenAPI 3 document generated from the catalog (tables,
columns, types, PK/FK) — `{"openapi":"3.0.3","info":{...},"paths":{...},
"components":{"schemas":{...}}}`, one `paths["/rest/v1/{table}"]` entry per
user table with `get`/`post`/`patch`/`delete` operations. No auth required
beyond the standard JWT bearer.

### C4 — GraphQL (`POST /graphql`, item 123 C4; mutations item 133)

A schema-derived, **read + write** GraphQL endpoint — the `pg_graphql`
analog, except this one also exposes **graph edge traversal** and **vector
similarity** as first-class fields, not just FK relationships (unidb's
differentiator over a relational-only Supabase/PostgREST stack). Mounted
under the same `require_jwt` layer as every other data-plane route. Source
of truth: `src/server/graphql.rs`.

```
POST /graphql
{"query": "{ ... }", "variables": {...}, "operationName": "..."}
-> {"data": {...}, "errors": [...]}   -- standard GraphQL-over-HTTP; always 200
```

**Schema-generation rules** (rebuilt from the live catalog on every request,
so a table created/dropped mid-session is visible on the next call):

- **One GraphQL `Object` type per table** (name = table name), skipping
  internal `__…__` tables and any table whose name isn't a valid GraphQL
  identifier (`/[_A-Za-z][_0-9A-Za-z]*/`) or collides with a reserved schema
  name (`Query`/`JSON`/`Edge`/`EdgeDirection`/the built-in scalar names).
- **One scalar field per column.** Type mapping: `INT` -> `Int`, `FLOAT` ->
  `Float`, `BOOL` -> `Boolean`, `TEXT`/`UUID`/`BYTEA`/`DATE`/`TIME`/
  `DECIMAL`/`TIMESTAMP` -> `String` (canonical rendering, same as `/rest/v1`'s
  JSON encoding), `JSON` -> a custom no-validator `JSON` scalar (arbitrary
  nested JSON passes through), `VECTOR(n)` -> `[Float!]`.
- **Root query field per table:** `<table>(<col>, <col>_neq, <col>_gt,
  <col>_gte, <col>_lt, <col>_lte, <col>_like, <col>_ilike, <col>_in,
  <col>_is_null, orderBy: String, limit: Int, offset: Int): [<Table>!]!` —
  the exact filter-operator matrix `/rest/v1` exposes as `<col>=<op>.<value>`
  (C1), spelled as distinct typed arguments. `orderBy` uses the same
  `"col.asc,col2.desc"` syntax as `/rest/v1`'s `order=`.
- **FK relationship fields**, resolved purely from catalog FK metadata (same
  pattern as C2's `resolve_relation`, generalized to enumerate every
  relationship up front): a **forward** (many-to-one) field aliased from the
  FK column (`customer_id` -> `customer`, nullable — `null` on a `NULL` FK
  value or an RLS/grant-hidden parent row) and a **reverse** (one-to-many)
  field aliased from the child table's name (non-null list, empty when the
  base key is `NULL` or no visible children exist). A name collision (rare —
  two FK columns/children resolving to the same alias) is disambiguated with
  an `_<fk_column>`/`_by_<fk_column>` suffix rather than dropped.
- **Graph edge traversal:** any table with a single `Int64` primary key gets
  an `edges(type: String, direction: EdgeDirection = OUT): [Edge!]!` field —
  `Edge { fromId: Int, toId: Int, edgeType: String, props: JSON }`, sourced
  from `__edges__` (the same storage `POST /edges`/`GET /edges/from/:id`
  use). `direction: IN` traverses edges where this row is `to_id` instead of
  `from_id`.
- **Vector similarity:** any table with a `VECTOR` column gets a **root**
  query field — `near_<table>(vector: [Float!]!, k: Int!): [<Table>!]!`, or
  `near_<table>_<col>` per column when a table has more than one `VECTOR`
  column. Root-level (not nested under a row) because a similarity search
  has no row to be relative to, unlike `edges`. Runs the identical `WHERE
  NEAR(column, [...], k)` predicate the SQL surface already supports —
  requires a `CREATE INDEX ... USING HNSW (col)` on the column first, same
  as `SELECT ... WHERE NEAR(...)` over `/sql`.
- **Introspection** (`__schema`/`__type`) is always enabled — point any
  standard GraphQL client/tool at `/graphql` to explore the live schema.

**Per-field authorization — the hard requirement:** every resolved field
(root query, FK forward/reverse, `edges`, `near`) runs a parameterized SQL
statement through the *exact same* enforced path `POST /sql`/`/rest/v1` use
(`authorize_sql_as_principal` + `execute_sql_params_as_principal` under the
caller's `AuthPrincipal`) — this module adds no parallel policy engine. A
row RLS hides comes back `null`/omitted, never leaked; requesting a column
the caller isn't granted denies that field with a `PERMISSION_DENIED`
GraphQL error (which — because every generated field type is non-null —
null-propagates up to `"data": null`, exactly like a denied column fails the
*whole* request over `/rest/v1`). `edges` traverses `__edges__` through this
same enforced path too, which is *stronger* than the lower-level `GET
/edges/from/:id` route (that route has no grant/RLS check at all — a bare
valid JWT is enough); a caller needs an explicit `GRANT SELECT ON
__edges__` to traverse edges via GraphQL. The query/`SELECT` a resolver
builds projects only the GraphQL sub-fields actually requested (mirroring
`/rest/v1`'s `select=`), not a blanket `SELECT *` — otherwise a
column-restricted caller's request for only their granted columns would be
denied by columns they never asked for.

**Injection safety:** filter/`vector`/`k` argument *values* are always typed
GraphQL arguments bound as `$n` parameters (or, for `NEAR`'s vector/`k` —
which must be SQL literals per this engine's grammar, not bind params —
formatted from already-parsed `f32`/`i64` values, never raw client text).
Identifiers (table/column/field names) come only from the catalog-derived
schema — an unrecognized `orderBy` column is rejected via the same
`validate_column` catalog check `/rest/v1` uses, never built into SQL text.

**Mutations (item 133) — a `Mutation` root alongside `Query`, one field set
per eligible table** (same eligibility filter as the query side; a schema
with zero eligible tables has no `Mutation` root at all, since a GraphQL
object type must have at least one field):

- **`insert_<table>(values: JSON!): <Table>`** — single-row insert, the
  analog of `POST /rest/v1/<table>`. `values`' JSON object keys are
  catalog-validated + quoted column names (an unknown key is the same
  `COLUMN_NOT_FOUND` REST returns); its values bind as `$n` parameters. Only
  a single JSON object is accepted in v1 (unlike REST's `POST`, which also
  takes a JSON array for a batch insert).
- **`update_<table>(<filter args>, set: JSON!): [<Table>!]`** — the analog of
  `PATCH /rest/v1/<table>?<filters>`. `<filter args>` is the exact same typed
  matrix the root query field exposes (`<col>`/`_neq`/`_gt`/`_gte`/`_lt`/
  `_lte`/`_like`/`_ilike`/`_in`/`_is_null`) minus `orderBy`/`limit`/`offset`
  (this engine's `UPDATE` grammar has no such concept). `set`'s JSON object
  follows the same column-validated / bind-parameterized rule as `values`.
  Returns every updated row.
- **`delete_<table>(<filter args>): [<Table>!]`** — the analog of `DELETE
  /rest/v1/<table>?<filters>`. Same filter-argument matrix as `update_<t>`.
  Returns every deleted row.

```graphql
mutation {
  insert_items(values: { id: 3, name: "cherry", price: 30 }) {
    id name price
  }
}
mutation {
  update_items(price_gt: 10, set: { price: 0 }) { id price }
}
mutation {
  delete_items(id: 2) { id name }
}
```

**Enforcement — identical to the query side, no new write path:** each
mutation resolver builds one `INSERT`/`UPDATE`/`DELETE ... RETURNING
<requested sub-fields>` statement and runs it through the exact same
`run_stmt`/`run_stmts` -> `authorize_sql_as_principal` +
`execute_sql_params_as_principal` path the query side, `/rest/v1`, and
`/sql` all share. `RETURNING`'s column list is authorized exactly like a
`SELECT` projection (`Engine::check_returning`) — a mutation's selection set
asking for a column the caller lacks a `SELECT` grant on is denied
identically to requesting that column via `/sql`'s own `RETURNING` clause,
even when the caller *does* hold the `INSERT`/`UPDATE` grant needed to write
it. `WITH CHECK`/RLS `FOR INSERT`/`FOR UPDATE`/`FOR ALL` policies apply on
the write path exactly as they do over `/sql` — a violating mutation is
rejected with the same `SQL_PLAN_ERROR` either way. See
`tests/item133_graphql_mutations.rs` for the parity proofs.

**Deferred (v1 scope, not built):** subscriptions, aggregations,
cursor-based pagination, combining `near`/`edges` with the root field's
filter/order/limit machinery, and mutation-side upsert/`on conflict` sugar.
Per-field resolution can N+1 (one enforced statement per resolved
field/row) — acceptable for a v1 whose correctness anchor is that every
field goes through the real enforced path.

---

## Storage service routes (item 31; per-object authorization item 120 F1)

Seven routes surface the `unidb-storage` app-layer crate as protected REST
endpoints. All require a JWT bearer token. All return
`503 {"error":"…","code":"STORAGE_NOT_AVAILABLE"}` when storage is not
configured (`STORAGE_BACKEND` env var absent or init failed at startup) — the
server boots cleanly without storage.

### Per-object authorization (item 120, Workstream F1)

Every object route (list/put/delete/presign) resolves the caller's identity
from the same JWT the rest of the server trusts (`AuthPrincipal` → `EngineHandle::
storage_caller`, reusing `authz::RoleStore`) and gates as follows:

- **Ownership.** The `objects` table's existing `created_by` column doubles as
  the object's owner: `put_object` stamps it from the caller's JWT `sub` claim.
  The owner may always read/write/delete their own objects.
- **Public buckets (read-only exemption).** `POST /storage/buckets` accepts
  `"is_public": true` (default `false` — private). Every object in a public
  bucket is readable (list / presign-GET) by **any** authenticated caller,
  regardless of ownership. Public status does **not** exempt writes or
  deletes — those are always owner-or-bypass-only.
- **Superuser / `service_role` bypass.** A named `SUPERUSER`, the implicit
  embedded/no-`sub` caller, open/bootstrap mode (no users registered), or a
  JWT carrying `"role": "service_role"` bypasses every rule — audited to
  `audit.log` as `superuser_storage_bypass` / `service_role_storage_bypass`
  (item-103 lesson: a bypass is provable, not just silent).
- **Fail closed.** A private bucket with no matching rule denies with
  `403 {"code":"STORAGE_FORBIDDEN"}`. `GET /storage/{bucket}/objects` never
  errors this way — it **filters** the listing to only the caller's readable
  objects.
- **Presign issuance is gated on the read rule.** `GET
  /storage/{bucket}/presign/{*key}` denies (403) for an object the caller
  could not otherwise read — a presigned URL grants bearer access to anyone
  holding it, so minting one for an unreadable object would be a bypass in
  itself.

This is a Rust-level owner/public-bucket/bypass gate over the existing
identity/role machinery, not a second policy-DDL evaluator; see
`docs/backlog/125_storage_per_object_authz.md` for the design note and the
policy-DDL follow-up it defers.

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

→ 200 { "buckets": [ { "name": "…", "created_by": "…"|null, "created_at_ms": N, "is_public": bool } ] }
→ 503 STORAGE_NOT_AVAILABLE
```

### C2 — Create bucket

```
POST /storage/buckets
Authorization: Bearer <token>
Content-Type: application/json
{ "name": "my-bucket", "is_public": false }   // is_public optional, defaults false

→ 201 (empty body)
→ 503 STORAGE_NOT_AVAILABLE
```

`is_public` (item 120, F1) governs the bucket's read-authorization exemption —
see "Per-object authorization" above. Repeating `create_bucket` for an
existing bucket is a no-op and does **not** update its `is_public` flag.

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
                    "created_at_ms":N, "owner":"…"|null } ],
    "prefixes": [ "photos/vacation/" ]   // virtual folders
  }
→ 503 STORAGE_NOT_AVAILABLE
```

With `prefix` + `delimiter`, objects whose key suffix (after the prefix) contains
the delimiter are folded into `prefixes` (virtual folders); the rest appear in
`objects`. Standard S3-style listing semantics.

**F1 (item 120):** the `objects` array is filtered to only the objects the
caller may read (public bucket, own objects, or a superuser/`service_role`
bypass) — this route never 403s, an unreadable object is simply absent.

### C5 — Put object (inline or presigned)

```
PUT /storage/{bucket}/objects/{*key}
Authorization: Bearer <token>
Content-Type: <mime-type>           (optional)
<body bytes>

→ 201 { "tier":"inline", "size":N, "etag":"…"|null }   // body.len() < inline_threshold
→ 200 { "presigned_put_url":"https://…", "storage_key":"…" }  // body.len() >= threshold
→ 403 STORAGE_FORBIDDEN   // overwriting another caller's object (F1)
→ 503 STORAGE_NOT_AVAILABLE
```

The split point is `StorageConfig::inline_threshold` (default 1 MiB). When the
body is below threshold, bytes are stored as an engine LOB in one transaction
(response 201). When at or above threshold, a pending metadata row is created and
a presigned PUT URL is returned (response 200); the client must PUT the bytes
directly to that URL.

**F1 (item 120):** creating a brand-new key is allowed for any authenticated
caller, who becomes its owner. Overwriting an **existing** key is gated to its
owner or a superuser/`service_role` bypass — public-bucket status does not
exempt writes.

### C6 — Delete object

```
DELETE /storage/{bucket}/objects/{*key}
Authorization: Bearer <token>

→ 204 (empty body)
→ 403 STORAGE_FORBIDDEN   // not the owner and no bypass (F1)
→ 404 STORAGE_NOT_FOUND
→ 503 STORAGE_NOT_AVAILABLE
```

### C7 — Presigned GET URL

```
GET /storage/{bucket}/presign/{*key}
Authorization: Bearer <token>

→ 200 { "presigned_get_url": "https://…" }
→ 403 STORAGE_FORBIDDEN   // caller cannot read this object (F1)
→ 404 STORAGE_NOT_FOUND
→ 503 STORAGE_NOT_AVAILABLE
```

Returns a time-limited URL for direct browser/client download without exposing
app credentials. **F1 (item 120):** issuance is gated on the same read rule as
a direct read — a caller cannot mint a presigned URL for an object they could
not otherwise read (public bucket, ownership, or a bypass).

### Storage error codes

| HTTP | code | meaning |
|------|------|---------|
| 503 | `STORAGE_NOT_AVAILABLE` | storage service not configured |
| 404 | `STORAGE_NOT_FOUND` | bucket or object does not exist |
| 403 | `STORAGE_FORBIDDEN` | per-object authorization denial (item 120, F1) — not the owner, the bucket isn't public, and no superuser/`service_role` bypass |
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
