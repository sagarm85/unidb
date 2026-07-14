# unidb Application Builder's Guide

> **What this is.** unidb is a storage/transaction *engine*, like Postgres — not
> an application. It does **not** ship application-shaped REST resources
> (`/users`, `/schema`, `/relationships`). It ships a documented **access +
> query + introspection surface**, and every application connects over that
> surface and builds its *own* REST/UI on top. This guide is the one document
> that tells you how to connect and extract every piece of data and metadata,
> the way the Postgres manual does. (Milestone 18, Epic E.)
>
> The forcing function was the `unidb-studio` console (schema visualizer / ERD,
> table+column search, DDL viewer). Everything it needs, it gets by `SELECT`ing
> from the catalog described in [§4](#4-introspect-the-system-catalog) — with no
> engine endpoint that knows the word "schema visualizer." That is the proof the
> surface is sufficient; the [30-line recipe](#6-recipe-a-schema-explorer-in-30-lines)
> is the living demo.

**Contents**

1. [Connect (access & auth)](#1-connect-access--auth)
2. [Query (the SQL surface)](#2-query-the-sql-surface)
3. [Bind parameters (`$n`)](#3-bind-parameters-n)
4. [Introspect (the system catalog)](#4-introspect-the-system-catalog)
5. [Results, types & paging](#5-results-types--paging)
6. [Recipe: a schema explorer in 30 lines](#6-recipe-a-schema-explorer-in-30-lines)
7. [Errors](#7-errors)
8. [Consume the event stream (change events)](#8-consume-the-event-stream-change-events)
9. [Honest limitations](#9-honest-limitations)
10. [Observe (metrics & health)](#10-observe-metrics--health)
11. [Store objects (the storage service)](#11-store-objects-the-storage-service)
12. [Logical replication and time-based PITR](#12-logical-replication-and-time-based-pitr-item-28)

---

## 1. Connect (access & auth)

*(Epic A.)* There are three access paths, and **one** query + catalog surface
reachable identically over all of them — a tool written against the catalog
works regardless of how it connects.

| Path | Crate / entry | For | Network |
|---|---|---|---|
| **Embed** | `unidb-embed` (`Engine` as a library) | in-process Rust apps | none — direct calls |
| **Attach** | `unidb-attach` (`AttachClient`) | out-of-process apps | HTTP(S) to a running server |
| **Server** | `unidb-server` over HTTP(S) | any language | thin `POST /sql` wrapper |

### Embed

```rust
use unidb::Engine;
let engine = Engine::open("/path/to/data_dir", 0)?; // 0 = default page size
let xid = engine.begin()?;
let results = engine.execute_sql(xid, "SELECT 1")?;
engine.commit(xid)?;
```

The embedded crate is the primary interface; there is no auth — you hold the
`Engine`, you have access. One `Engine` owns one data directory (one database).

### Server (access-token URL)

Start `unidb-server` (see `docs/REST_API.md` for the full route reference). The
connection contract:

- **Base URL**: `http://<host>:<port>` (or `https://…` when TLS is configured —
  `axum-server` + rustls, no OpenSSL). Default bind is **`127.0.0.1:8080`**
  (`UNIDB_BIND_ADDR`).
- **Auth token placement**: an HS256 JWT in the **`Authorization: Bearer <jwt>`**
  header, on **every** request. The server is **verify-only** — it does not
  issue tokens (an external auth service or `scripts/gen_jwt.sh` signs them with
  the shared `UNIDB_JWT_SECRET`). There is **no** `?token=` query-parameter form.
- **One database per server**: the instance serves the single database under
  `UNIDB_DATA_DIR`. There is **no** multi-db path addressing (`/<db>`); to serve
  several databases, run several instances.
- **Browser caveat**: a browser cannot open a raw socket, so a browser SPA either
  talks to this server's `POST /sql` over HTTPS (the generic query + catalog
  surface — *not* app resources), or runs its own backend-for-frontend that
  embeds/attaches and serves app-shaped REST to its own frontend. Either way the
  boundary holds: **the engine stops at a generic query + catalog surface; the
  application owns its REST.**

```bash
curl -sS https://db.example.com:8080/sql \
  -H "Authorization: Bearer $UNIDB_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"sql":"SELECT table_name FROM information_schema.tables"}'
```

### Attach (Rust client)

```rust
use unidb_attach::AttachClient;
let client = AttachClient::new("https://db.example.com:8080", &jwt_token)?;
let results = client.execute_sql("SELECT 1")?;
```

`AttachClient::new` takes the **base URL and the JWT separately** — you pass
`"https://host:port"` and the token string; it sets `Authorization: Bearer …`
for you. A single compact `unidb://<token>@<host>:<port>/<db>` DSN string is
**not** parsed today; assemble the base URL + token yourself. (`AttachClient`
is blocking — `reqwest::blocking`.)

### Sessions vs. one-shot requests

*(A2.)* Over the server/attach path, a request with **no** `X-Txn-Id` header is a
single self-contained auto-commit transaction (a `;`-separated `/sql` body is
still atomic as a whole). Opening a session with `POST /txn/begin` and passing
`X-Txn-Id` makes subsequent statements run inside that transaction until you
commit/rollback. Full session rules and isolation levels are in
`docs/REST_API.md` → *Transaction sessions*. The embed equivalent is explicit
`begin()` / `commit()` / `abort()` around `execute_sql`.

---

## 2. Query (the SQL surface)

*(B1.)* unidb speaks a practical SQL subset (not full ANSI). Send it as text over
any access path; the same parser/planner/executor runs underneath all three.

**Supported**

- **DDL**: `CREATE TABLE` (with `PRIMARY KEY`, `UNIQUE`, `NOT NULL`, `CHECK`,
  `DEFAULT`, `FOREIGN KEY … REFERENCES`, `SERIAL`/identity), `CREATE INDEX …
  USING {BTREE|HNSW|FULLTEXT}`, `ALTER TABLE ADD/DROP COLUMN`, `DROP TABLE`,
  `TRUNCATE`, `ANALYZE`.
- **DML**: `INSERT` (multi-row), `UPDATE`, `DELETE`, all `WHERE`-filtered.
- **SELECT**: projections, `WHERE` (including `LIKE` / `NOT LIKE` / `ILIKE`
  pattern matching and `MATCH(col, 'text')` full-text boolean predicate — item 30,
  G9 + G11), `INNER`/`LEFT`/`RIGHT`/`CROSS JOIN` with `ON <expr>` **or `USING
  (cols)`** (shared columns merged per standard SQL), `GROUP BY` / `HAVING`,
  aggregates (`COUNT`/`SUM`/`AVG`/`MIN`/`MAX`), `DISTINCT`, `ORDER BY`,
  `LIMIT`/`OFFSET`, subqueries (`IN`, `EXISTS`, scalar, correlated), and CTEs
  (`WITH`).
- **Transactions**: `READ COMMITTED` (default), `REPEATABLE READ`,
  `SERIALIZABLE` (SSI).
- **`EXPLAIN [ANALYZE]`**: renders the chosen plan (and, with `ANALYZE`, actual
  row counts + timing).
- **Domain extensions**: `VECTOR(n)` columns + `NEAR` similarity, full-text
  search (index via `CREATE INDEX … USING FULLTEXT`; predicate via `MATCH(col,
  'text')` in `WHERE`), graph edges (Cypher subset via a separate entry point), a
  WAL-derived event queue.
- **`NEAR` relevance score (item 41):** project the virtual column
  `vec_distance` alongside a `NEAR(...)` predicate to get the exact
  re-ranked Euclidean distance for each row — `SELECT id, title, vec_distance
  FROM documents WHERE NEAR(embedding, [...], k)` returns rows ascending by
  distance (closest first) as `Float`. It only resolves inside a `NEAR`
  query's projection; `SELECT vec_distance FROM t` without a `NEAR` predicate,
  or `SELECT *`, never surfaces it (`COLUMN_NOT_FOUND`/omitted respectively) —
  it is not a real catalog column.

**Not supported yet** — so you don't guess:

- `NATURAL JOIN` — use explicit `JOIN … USING (cols)` or `ON`.
- `FULL OUTER JOIN` (needed for a true `COALESCE`-merged `USING` column on both
  outer sides; `INNER`/`LEFT`/`RIGHT` `USING` are supported).
- `ORDER BY` on an expression that is **not** in the SELECT output list (order by
  a projected column name/alias or ordinal position).
- Set operations (`UNION`/`INTERSECT`/`EXCEPT`), window functions, `RETURNING`.
- `ON DELETE CASCADE / SET NULL / NO ACTION` and `ON UPDATE` FK actions — only
  `RESTRICT` (the default) is enforced today. `CASCADE`/`SET NULL` are parsed
  but not yet acted on.
- `SELECT` without `FROM` (`SELECT 1` alone) over the row-at-a-time path.

For the exhaustive, always-current truth, the parser (`src/sql/parser.rs`)
returns a `SQL_UNSUPPORTED` error naming what it rejected.

---

## 3. Bind parameters (`$n`)

*(B2 — already shipped; the safe way to build filters/search.)* Use `$1..$n`
placeholders and pass values **positionally**; each value is bound as **data**,
never re-parsed as SQL — this is what makes it injection-proof. A value that
would be malicious inside an interpolated string (`"'; DROP TABLE t; --"`) binds
as a plain text literal and can only ever match that literal string.

- **Embed**: `engine.execute_sql_params(xid, "… WHERE name = $1 AND age > $2",
  &[Literal::Text("alice".into()), Literal::Int(30)])`.
- **Server / attach**: add a `params` array to the `/sql` body:
  ```json
  { "sql": "INSERT INTO t (id, name) VALUES ($1, $2)", "params": [1, "alice"] }
  ```

**Coercion rules**: a JSON string binds as text and is later coerced to the
target column's type (UUID, TIMESTAMP, DATE, …); a JSON number binds as int or
float; a JSON numeric array binds as a `VECTOR`. Omitting `params` (or an empty
array) runs the SQL as-is. Prepared statements (parse once, execute many) are
available via `Engine::prepare` / `execute_prepared`.

---

## 4. Introspect (the system catalog)

*(Epic C — the heart of this milestone.)* Introspection is exposed as
**relations you `SELECT` from over the ordinary query surface** — no bespoke
endpoints. The relations are **synthesized on demand** from the live catalog, so
they are always current; they are not on-disk tables (no vacuum/MVCC
interaction) and are reachable identically from embed, attach, and server. Names
mirror `information_schema` so Postgres knowledge transfers directly; unidb-native
extensions live under `unidb_catalog`.

Engine-internal `__…__` tables are hidden. Every `*_schema` column reports the
constant `'public'` and every `*_catalog` column reports `'unidb'` — unidb has
no schema namespacing, and saying so plainly beats inventing one.

### Relations

| Relation | Story | Key columns |
|---|---|---|
| `information_schema.tables` | list tables | `table_name`, `table_type` (`BASE TABLE`) |
| `information_schema.columns` | list columns | `column_name`, `data_type`, `is_nullable` (`YES`/`NO`), `ordinal_position`, `column_default` |
| `information_schema.table_constraints` | PK / UNIQUE / CHECK / FK per table | `constraint_name`, `constraint_type` |
| `information_schema.key_column_usage` | columns participating in a key constraint, ordered | `constraint_name`, `column_name`, `ordinal_position`, `position_in_unique_constraint` |
| `information_schema.referential_constraints` | FK → referenced-key link | `constraint_name`, `unique_constraint_name`, `update_rule`, `delete_rule` |
| `unidb_catalog.indexes` | secondary indexes | `table_name`, `column_name`, `index_type`, `is_unique` |

**Constraint names are synthesized** deterministically (unidb does not store
named constraints), Postgres-style: `<table>_pkey`, `<table>_<col>_key`
(UNIQUE), `<table>_<cols>_fkey`, `<table>_<col>_check`. They are stable across
reopens.

### Worked examples

List tables and columns (drives table search + the column grid):

```sql
SELECT table_name, column_name, data_type, is_nullable, ordinal_position
FROM   information_schema.columns
WHERE  table_schema = 'public';
```

Enumerate real foreign keys to draw ERD edges + FK badges — **no name-heuristic
guessing.** Shown in `ON` form here; `JOIN … USING (constraint_name)` also works.
The `ccu.ordinal_position = kcu.position_in_unique_constraint` conjunct aligns
each FK column with its referenced column for **composite** keys:

```sql
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

### Object DDL — reconstruct from metadata (C5)

unidb does **not** retain the original `CREATE …` text, and there is no
`object_ddl(name)` table-function. Reconstruct DDL from the relations above — the
same data a "View DDL" button needs, built client-side:

1. **Columns** — `information_schema.columns` gives `column_name`, `data_type`,
   `is_nullable` (append `NOT NULL` when `NO`), `column_default` (append
   `DEFAULT <value>` when non-NULL), in `ordinal_position` order.
2. **Primary key** — `key_column_usage` rows for the `…_pkey` constraint, in
   `ordinal_position` order → `PRIMARY KEY (c1, c2, …)`.
3. **Unique** — each `UNIQUE` constraint's `key_column_usage` columns →
   `UNIQUE (…)`.
4. **Foreign keys** — `referential_constraints` + the two `key_column_usage`
   joins above → `FOREIGN KEY (from_cols) REFERENCES to_table (to_cols)`.
5. **Indexes** — `unidb_catalog.indexes` → `CREATE INDEX … USING <index_type>
   (<column_name>)`.

This reconstruction is **canonical, not byte-identical** to the original DDL:
whitespace is normalized, `CHECK` expressions and `DEFAULT` values are re-rendered
from their parsed form, and synthesized constraint names replace anonymous ones.

---

## 5. Results, types & paging

### Result column metadata (D1)

Every `rows` result carries a **`columns`** array of the output column **names**,
so a grid renders without a second round-trip — including for projections, joins,
aggregates (aliased or synthesized names), and `EXPLAIN` (one `QUERY PLAN`
column). The catalog is the source of column *types*: the `rows` envelope carries
names, not types, so read `information_schema.columns.data_type` when you need the
declared type. (Embed: `ExecResult::Rows { columns, rows }`. Server/attach: the
`{ "type": "rows", "columns": [...], "rows": [...] }` shape.)

### Type ↔ representation mapping (B3)

How each engine type appears in `information_schema.columns.data_type` and on the
JSON wire (`POST /sql` results / attach):

| Engine type | `data_type` string | JSON / wire representation |
|---|---|---|
| `INT` / `BIGINT` | `bigint` | JSON number |
| `TEXT` | `text` | JSON string |
| `BOOL` | `boolean` | JSON `true`/`false` |
| `JSON` | `json` | **nested** JSON value (never a JSON-encoded string) |
| `DECIMAL(p,s)` | `numeric(p,s)` | **decimal string**, e.g. `"9.90"` (exact — no float error) |
| `TIMESTAMP` | `timestamp` | UTC timestamp string |
| `DATE` | `date` | date string |
| `TIME` | `time` | time-of-day string |
| `FLOAT` / `DOUBLE` | `double precision` | JSON number |
| `UUID` | `uuid` | canonical lowercase hyphenated string |
| `BYTEA` | `bytea` | hex string (`\xDEADBEEF`) |
| `VECTOR(n)` | `vector(n)` | JSON array of `n` numbers |
| any NULL | — | JSON `null` |

`DECIMAL`-as-string and `JSON`-as-nested-value are the two that trip people up;
both are deliberate (see `docs/REST_API.md` → `POST /sql`).

### Pagination (D2)

- **`LIMIT` / `OFFSET`** works for small pages. Beware deep `OFFSET`: it still
  scans and discards the skipped rows.
- **Keyset pagination** — for large ordered scans, page by the last seen key
  (`WHERE id > $last ORDER BY id LIMIT 100`), which stays O(page) instead of
  O(offset).
- **Server cursors** — for streaming a very large result without materializing it
  client-side, `POST /sql` with `"cursor": true` returns a `cursor_id`; page it
  with `GET /sql/cursor/{id}?limit=N`. See `docs/REST_API.md` → cursors (R4).

---

## 6. Recipe: a schema explorer in 30 lines

A complete ERD/schema explorer built on **only** the documented surface — the
proof the contract is sufficient (the studio is the production version of this).

```python
import requests

BASE, TOKEN = "https://db.example.com:8080", "<jwt>"
def sql(q):
    r = requests.post(f"{BASE}/sql",
                      headers={"Authorization": f"Bearer {TOKEN}"},
                      json={"sql": q})
    r.raise_for_status()
    res = r.json()["results"][0]
    return [dict(zip(res["columns"], row)) for row in res["rows"]]

# 1. tables + their columns  (nodes)
tables = {t["table_name"]: [] for t in sql(
    "SELECT table_name FROM information_schema.tables")}
for c in sql("""SELECT table_name, column_name, data_type, is_nullable
                FROM information_schema.columns WHERE table_schema='public'"""):
    tables[c["table_name"]].append(c)

# 2. foreign keys  (edges) — real, not guessed
edges = sql("""
  SELECT tc.table_name AS src, kcu.column_name AS src_col,
         ccu.table_name AS dst, ccu.column_name AS dst_col
  FROM information_schema.table_constraints tc
  JOIN information_schema.key_column_usage kcu
       ON kcu.constraint_name = tc.constraint_name
  JOIN information_schema.referential_constraints rc
       ON rc.constraint_name = tc.constraint_name
  JOIN information_schema.key_column_usage ccu
       ON ccu.constraint_name = rc.unique_constraint_name
      AND ccu.ordinal_position = kcu.position_in_unique_constraint
  WHERE tc.constraint_type = 'FOREIGN KEY'""")

# 3. render
for name, cols in tables.items():
    print(name, "→", ", ".join(f"{c['column_name']}:{c['data_type']}" for c in cols))
for e in edges:
    print(f"  {e['src']}.{e['src_col']} ──▶ {e['dst']}.{e['dst_col']}")
```

---

## 7. Errors

*(B4.)* Errors are a `{ "error": <message>, "code": <MACHINE_READABLE_CODE> }`
envelope with a stable `code` you can branch on. The full HTTP-status ↔ `code`
table is the documented contract in `docs/REST_API.md` → **Error codes**
(e.g. `TABLE_NOT_FOUND` 404, `UNIQUE_VIOLATION` 409, `SERIALIZATION_FAILURE`
409, `SQL_UNSUPPORTED` 400, `SQL_PARSE_ERROR` 400). Embed callers get the same
distinctions as typed `DbError` variants.

---

## 8. Consume the event stream (change events)

*(Milestone 20, Epic E.)* Opt a table into **event capture** and every
committed `INSERT`/`UPDATE`/`DELETE` on it also appends a change event —
**captured atomically in the very same WAL append/commit as the write itself**.
There is no Debezium-style log tailer, no separate CDC connector, and therefore
no split-brain or capture lag: if the row is committed, its event is committed,
in one transaction, in one log. This is the primitive the studio "Events" tab
and any downstream service consume.

Enable it once per table:

| Path | Call |
|---|---|
| **Embed** | `engine.enable_events("orders")` |
| **Attach** | `client.enable_events("orders")` |
| **Server** | `POST /tables/orders/events` |

### 8.1 The event payload (the stable contract)

Every event is a row of the engine-managed `__events__` table, delivered as this
JSON object. These field names are the contract (item 29, C1):

| Field | Type | Meaning |
|---|---|---|
| `seq` | integer | **Monotonic per-database offset**, assigned in commit order. This *is* the cursor/"LSN" of the stream: consumers resume from, ack, and dedupe on `seq`. Dense and gap-free across enabled tables. |
| `xid` | integer | Transaction id that produced the event. All events sharing an `xid` were committed atomically together (use it to reassemble a multi-row/multi-table transaction). |
| `table_name` | string | Source table. |
| `op` | string | `"insert"`, `"update"`, or `"delete"`. |
| `payload` | object | **Back-compat flat row image**: post-image for INSERT/UPDATE, pre-image for DELETE. New consumers should prefer `before`/`after` below. |
| `before` | object \| null | **Pre-mutation row image** (null for INSERT). Present for UPDATE and DELETE. |
| `after` | object \| null | **Post-mutation row image** (null for DELETE). Present for INSERT and UPDATE. |
| `ts_ms` | integer | Capture wall-clock in Unix epoch milliseconds. 0 for events written before item 29. |

> **Back-compat note.** `payload` is kept for consumers written against the
> pre-item-29 contract — it holds the same flat `{col: val}` object it always
> did. No existing consumer that reads `event.payload.col` needs to change.
> New consumers should read `before`/`after` for correct before/after semantics
> on UPDATE (today `payload` = after for UPDATE, which was always ambiguous for
> a consumer that needs the old value too).

> **Honest scope of the contract.** `seq` is the ordering key and offset — unidb
> does **not** expose the physical WAL byte-LSN as a separate event field (it
> would leak a storage-internal number with no consumer use that `seq` doesn't
> already serve). `source.lsn` is a documented follow-up if commit-time wiring
> is added. The engine emits raw row-level facts and transforms nothing — all
> shaping is consumer-side.

### 8.2 Wire formats (item 29, C2)

`GET /events/subscribe` accepts `?format=<name>` to request one of three wire
shapes. **`seq` stays the SSE `id:` frame in every format.** Format is
per-connection, not per-event.

#### `?format=native` (default)

The full `Event` struct as JSON. Carries every field including `before`, `after`,
and `ts_ms`. Recommended for new consumers — no information loss.

```json
{
  "seq": 42, "xid": 1017, "table_name": "orders", "op": "update",
  "payload": {"id": 1, "status": "shipped"},
  "before": {"id": 1, "status": "pending"},
  "after":  {"id": 1, "status": "shipped"},
  "ts_ms": 1752000000000
}
```

#### `?format=debezium`

Debezium-compatible envelope. Single-char op (`c`/`u`/`d`). Compatible with
Kafka-Connect Debezium sinks that consume the Debezium JSON payload format.

```json
{
  "payload": {
    "op": "u",
    "ts_ms": 1752000000000,
    "before": {"id": 1, "status": "pending"},
    "after":  {"id": 1, "status": "shipped"},
    "source": {"seq": 42, "txId": 1017, "table": "orders", "schema": "public"}
  }
}
```

Op mapping: `insert` → `c`, `update` → `u`, `delete` → `d`.

#### `?format=supabase`

Supabase Realtime-compatible flat envelope. Compatible with consumers written
against the Supabase CDC wire format.

```json
{
  "eventType": "UPDATE",
  "new": {"id": 1, "status": "shipped"},
  "old": {"id": 1, "status": "pending"},
  "schema": "public",
  "table": "orders",
  "commit_timestamp": "2026-07-13T12:00:00.000Z"
}
```

#### Non-goals (V1)
- Kafka transport: not a goal; SSE + `unidb-dispatch` webhook cover it.
- Subscription-level RLS (row filtering by subscriber policy): depends on item 24.
- `source.lsn`: requires commit-time wiring; `seq` is the ordering cursor for V1.

### 8.3 Consuming: durable consumers vs. live tail

Two models, one stream:

- **Durable consumer (at-least-once).** Poll with a named consumer; the engine
  tracks that consumer's **durable** offset. Process the batch, then `ack` up to
  the last `seq` you handled. A crash before ack ⇒ redelivery (dedupe on `seq`).
  This is the Kafka manual-commit shape.
  - Embed: `poll_events(xid, "billing", limit)` → process → `ack_events(xid, "billing", up_to_seq)`.
  - Server: `GET /events/subscribe?consumer=billing` (SSE) → `POST /events/ack`.
    Acks travel over `/events/ack`, never over the SSE connection.
    Append `?format=debezium` or `?format=supabase` to switch wire format (§8.2);
    `seq` remains the SSE `id:` frame and the ack cursor in all formats.
- **Ephemeral live tail (at-most-once).** For a browser `EventSource` that just
  wants to watch a table: `GET /events/subscribe` with **no** `consumer`. Nothing
  is written to the consumer registry. Resume-from-offset on reconnect is the
  standard SSE `Last-Event-ID` header (each frame carries `id: <seq>`), or an
  explicit `?from_seq=<seq>` for offset scrubbing / replay-from-offset. An
  optional `?table=<name>` filters to one table. Heartbeats keep the idle
  connection open.

### 8.4 Replay & the vacuum-horizon contract

Events are ordinary durable rows, so **replay is just reading from an earlier
offset**: a durable consumer that acks a lower `seq` (or a fresh consumer that
never acked) re-receives history; an ephemeral tail replays from any `from_seq`.

Retention is bounded by the **all-consumers vacuum horizon**: `vacuum_events`
(embed) / `POST /events/vacuum` (server) reclaims only events every *registered*
consumer has already acked past — never automatically. The consequences a
consumer must respect:

- A **slow or stopped durable consumer pins retention**: its un-acked events
  cannot be vacuumed, so `__events__` grows until it catches up. Monitor lag and
  surface "consumer too far behind" loudly (the reference dispatcher does — see
  below); a consumer you will never resume should be dropped so it stops holding
  the horizon.
- **Vacuuming past an offset makes that history unreplayable.** Don't vacuum
  below the earliest offset any consumer (or planned replay) still needs.
- A **not-yet-registered** consumer has no offset, so `vacuum_events` cannot
  account for it — register (first `ack`) before you rely on full history.

### 8.5 The reference dispatcher (`unidb-dispatch`)

`unidb-dispatch` (workspace crate) is the app-layer fan-out service: it embeds
the engine, consumes from a **durable offset**, and fans events out to webhooks
(retry with exponential backoff, then **dead-letter into a unidb table** —
dogfood) and in-process rooms (the primitive a WebSocket/SSE room layer
subscribes to), with per-subscription table/op filters and column projection.
It adds **no engine surface** — it only drives the calls above — and it keeps
`tokio`/`reqwest` out of the engine's default (sync) build. Delivery is
at-least-once; a failing endpoint is retried then dead-lettered while the offset
still advances, so a poison event cannot wedge the stream.

> The studio **"Events" tab** (live viewer, offset scrubbing, replay-from-offset,
> per-table enable/disable) is built on exactly these routes and lives in the
> `unidb-studio` repo — out of scope for the engine, by design.

### 8.6 Lag observability & detection (item 29, C3)

The engine exposes per-consumer lag through three surfaces that all read the
same underlying counters:

**Virtual relation (embed or SQL):**

```sql
SELECT * FROM unidb_catalog.subscription_lag;
-- consumer | offset | max_seq | lag_events | oldest_unconsumed_ts_ms | lag_seconds
```

| Column | Description |
|---|---|
| `consumer` | registered consumer name |
| `offset` | last acked `seq` |
| `max_seq` | highest seq in `__events__` (O(log n) B-tree lookup) |
| `lag_events` | `max_seq − offset` |
| `oldest_unconsumed_ts_ms` | `ts_ms` of the first un-acked event (0 if caught up) |
| `lag_seconds` | `(now − oldest_unconsumed_ts_ms) / 1000.0` (0.0 if caught up) |

**`/stats` JSON (server, item 21):**

```json
{
  "subscription_lag": [
    { "consumer": "billing", "offset": 41, "max_seq": 50,
      "lag_events": 9, "oldest_unconsumed_ts_ms": 1720000000000,
      "lag_seconds": 3.7 }
  ]
}
```

**Prometheus gauges (scraped from `/metrics`):**

```
unidb_subscription_lag_events{consumer="billing"} 9
unidb_subscription_lag_seconds{consumer="billing"} 3.7
```

**Alert guidance.** A useful starting point:

- Alert on `unidb_subscription_lag_events{consumer="X"} > 1000` for latency-sensitive
  consumers.
- Alert on `unidb_subscription_lag_seconds{consumer="X"} > 30` for near-real-time
  pipelines.
- A consumer that has stopped acking but whose offset is still referenced by
  `vacuum_events` will pin the `__events__` table indefinitely — drop it
  (`DELETE FROM __consumers__ WHERE name = 'X'`) if it will never resume.

---

## 9. Honest limitations

- **FK enforcement** (item 36): child INSERT/UPDATE verifies the referenced
  parent key exists (O(log n) via the parent's implicit DiskBTree; heap-scan
  fallback for composite FKs). Parent DELETE/UPDATE enforces **RESTRICT** — a
  parent row cannot be deleted/updated while a visible child references it.
  `ON DELETE CASCADE / SET NULL` and `ON UPDATE` actions are not yet
  implemented; the catalog reports `update_rule`/`delete_rule` as `NO ACTION`.
  A composite FK without a matching secondary index on the child FK column uses
  an O(n) heap scan for the RESTRICT check (documented limitation).
- **Constraint names are synthesized, not stored.** `<table>_pkey` etc. — stable
  and deterministic, but not names you declared (unidb has no named-constraint
  syntax).
- **Reconstructed DDL is canonical, not byte-identical** (see [§4](#object-ddl--reconstruct-from-metadata-c5)).
- **`is_unique` on `unidb_catalog.indexes`** reflects whether the indexed column
  carries a UNIQUE/PK *constraint*, not a property of the index itself — unidb
  secondary indexes are non-unique structures; uniqueness is enforced by the
  constraint, not the index. Each `PRIMARY KEY` / `UNIQUE` column also has an
  **implicit internal B-tree** auto-created at `CREATE TABLE` time (item 35)
  that backs the O(1) uniqueness check — this internal index is *not* surfaced
  in `unidb_catalog.indexes` (only explicit `CREATE INDEX` indexes appear there).
- **One schema, one database per server.** `'public'` / `'unidb'` are constants;
  there is no schema namespacing and no multi-db addressing.
- **No `NATURAL JOIN` / `FULL OUTER JOIN`** (`JOIN … USING` *is* supported —
  `INNER`/`LEFT`/`RIGHT`; see [§2](#2-query-the-sql-surface)).

- **Correction (item 27, supersedes the item-21-era limitation formerly stated
  here):** dead/live-tuple pressure **is now per table** —
  `tables[].{dead_tuple_estimate,live_tuple_estimate}` in `GET /stats`/
  `Engine::stats()`, and `Engine::vacuum_table(name)`/`tables_needing_vacuum()`
  let autovacuum target only the table that churned instead of a full-engine
  pass. What's still engine-global, honestly: (1) the flat top-level
  `dead_tuple_estimate`/`live_tuple_estimate` fields, which now cover only
  raw-CRUD heap writes with no table name to attribute to; (2) the Prometheus
  `/metrics` facade, which republishes those two as engine-global gauges only
  — the per-table breakdown is JSON-only via `GET /stats`/`Engine::stats()`
  today, not yet mirrored as per-table Prometheus gauges.
- **Histogram percentiles are log-bucket estimates, not exact quantiles**
  (item 21). `p50_us`/`p99_us` are the **upper bound** of the power-of-two
  bucket the rank falls in (the Prometheus `le` convention) — a safe
  over-estimate for an SLO panel, never an under-estimate.

---

## 10. Observe (metrics & health)

*(Item 21, grown by items 27 and 29.)* Every production metric is captured
**lock-free** on the hot path (plain atomics + a fixed-bucket atomic
histogram — no mutex on the commit or scan path) and surfaced **only**
through the documented boundaries — item 21's original chokepoint metrics,
item 27's per-table vacuum accounting, and item 29's per-consumer CDC lag all
grew the *same* two surfaces below; none of them opened a new endpoint:

- **`Engine::stats()`** (embed) / **`GET /stats`** (server) — one JSON snapshot,
  the `EngineStats` shape. The server adds three session gauges the engine can't
  see (`open_txn_sessions`, `open_cursors`, `idle_reaper_aborts`).
- **`GET /metrics`** (server) — the same values republished through the
  Prometheus facade on each scrape (a scrape never perturbs the write path).

A Studio "Observability" tab renders the widgets below from these two surfaces
alone — no bespoke endpoint (the Milestone-18 boundary).

### Widget-traceability table

Every widget maps to a named, documented metric. `stats()` JSON path on the
left, Prometheus metric on the right; units are microseconds unless noted.

| Widget (panel) | `stats()` JSON field | Prometheus metric | Captured at |
|---|---|---|---|
| Query latency — per kind | `statement_latency.{insert,update,delete,select}.{p50_us,p99_us,mean_us,count}` | `unidb_statement_latency_p50_us{kind}`, `unidb_statement_latency_p99_us{kind}`, `unidb_statement_count{kind}` | `execute_one_plan` (per SQL statement) |
| Commits/s | `commits`, `aborts` | `unidb_commits_total`, `unidb_aborts_total` | `Engine::commit`/`abort` |
| Durability cost | `wal_fsyncs`, `wal_fsync_latency.{p50_us,p99_us}` | `unidb_wal_fsyncs_total`, `unidb_wal_fsync_p50_us`, `unidb_wal_fsync_p99_us` | `Wal::sync` / `group_fsync` (around `sync_all`) |
| Cache efficiency | `bufferpool.{hits,misses,evictions,hit_ratio}` | `unidb_bufferpool_hits_total`, `unidb_bufferpool_misses_total`, `unidb_bufferpool_evictions_total`, `unidb_bufferpool_hit_ratio` | `BufferPool::fetch_page`/`find_victim` |
| Contention | `locks.{waits,deadlocks,wait.p50_us,wait.p99_us}` | `unidb_lock_waits_total`, `unidb_deadlocks_total`, `unidb_lock_wait_p50_us`, `unidb_lock_wait_p99_us` | `LockManager::acquire` (blocking-wait path) |
| **Bloat risk (alertable)** | `horizon_age_secs` | `unidb_horizon_age_seconds` | `TransactionManager` (oldest live snapshot age) |
| Table health | `tables[].{name,pages,dead_tuple_estimate,live_tuple_estimate}` (per-table, item 27) | `unidb_table_pages{table}` (per-table); dead/live-tuple gauges are **engine-global only** on this facade — `unidb_dead_tuple_estimate`, `unidb_live_tuple_estimate` | catalog + heap page directory (cold, on read) |
| Autovacuum | `autovacuums`, `last_autovacuum_epoch_secs` | `unidb_autovacuum_runs_total`, `unidb_autovacuum_last_run_epoch_secs` | autovacuum launcher (A4); item 27 added `Engine::vacuum_table` so a trigger can target one table without a full pass |
| Worker governance | `parallel_workers.{global_max,available,parallel_scans,workers_granted,serial_fallbacks}` | `unidb_parallel_worker_budget`, `unidb_parallel_workers_available`, `unidb_parallel_scans_total`, `unidb_parallel_workers_granted_total`, `unidb_parallel_serial_fallbacks_total` | `parallel_scan::acquire` (admission) |
| Server sessions | `open_txn_sessions`, `open_cursors`, `idle_reaper_aborts` *(server-only)* | `unidb_open_txn_sessions`, `unidb_open_cursors`, `unidb_idle_reaper_aborts_total` | session/cursor registries + idle reaper |
| Replication lag | `replication_slots`, `max_replication_lag` | *(via `/stats`)* | slot registry |
| **CDC/event lag (item 29)** | `subscription_lag[].{consumer,offset,max_seq,lag_events,oldest_unconsumed_ts_ms,lag_seconds}` | `unidb_subscription_lag_events{consumer}`, `unidb_subscription_lag_seconds{consumer}` | computed on read from `__consumers__` + the durable event-order index; also queryable directly as `unidb_catalog.subscription_lag` |

**The horizon-age gauge is the one to alert on.** A pinned vacuum horizon (an
idle `REPEATABLE READ` session, an abandoned open transaction, a slow reader) is
the #1 silent cause of table bloat and scan slowdown — the item-16 postmortem
metric. `horizon_age_secs` climbs for as long as the oldest live snapshot is
held and drops to `0` the instant it commits/aborts; on the server, the
idle-session reaper caps the worst case and increments
`idle_reaper_aborts` when it does.

---

## 11. Store objects (the storage service)

*(Item 23.)* Storing files is an **access pattern over the engine**, not an engine
feature: the `unidb-storage` app-layer crate keeps bucket/object **metadata** in
ordinary unidb tables and tiers object **bytes** between engine LOBs (small,
ACID-inline) and an S3-wire object store (MinIO/S3, large). It adds **no** engine
surface — same boundary as `unidb-dispatch`.

- **Two tiers, one API.** `put_object` routes by size: `< inline_threshold`
  (default 1 MiB) → an engine **LOB written in the same transaction as the
  metadata row** (commit/rollback atomic — the P3.d edge); larger → the object
  store. `get_object`/`delete_object` are tier-transparent.
- **Presigned URLs.** For large objects a browser moves bytes **directly**:
  `begin_upload` returns a presigned PUT (and writes a `pending` metadata row);
  `presign_get` returns a download URL. The engine never proxies a large payload.
- **Outbox + reconciler (consistency).** The `pending` metadata row and its
  `objects` **insert event** commit atomically (§8) — that event *is* the outbox.
  A `Reconciler` then **confirms** uploads (`pending → ready` once the bytes
  land), **compensates** stale pending rows (`pending → failed` + a dead-letter
  row — never a dangling pending), and **sweeps** orphaned store bytes. So a
  crash mid-upload leaves neither a metadata row without bytes nor unreferenced
  bytes.
- **Consuming the confirm stream.** Because `objects` has events enabled, a
  downstream service can subscribe to it exactly like any other table (§8) — the
  reference `ConfirmSink` does this via a real `unidb_dispatch::Dispatcher`.

Config, env vars, and the (Docker-free) testing story live in
`unidb-storage/README.md`; the design rationale (S3 client choice, the
outbox/reconciler decision, and the single-page **catalog ceiling** that shaped
the schema) is in `docs/design/storage_service.md`. The studio **"Storage" tab**
is out of this repo (like Events/Logs), by design.

---

*Milestone 18 (engine access & introspection contract) + item 21 (observability
metrics enrichment, §10, since grown by items 27 and 29). See
`docs/backlog/18_engine_access_contract.md` and
`docs/backlog/21_observability_metrics.md` for the specs, and `docs/REST_API.md`
for the exhaustive HTTP route/error reference. §8 (event stream / change events)
was added by Milestone 20 — `docs/backlog/20_events_realtime_dispatcher.md`; §11
(object storage) by item 23 — `docs/backlog/23_storage_service.md`.*

---

## 13. Observability API gaps (item 34)

### Slow-query threshold (Part A)

Set at server startup via `UNIDB_SLOW_QUERY_MS=100` (absent or `0` = disabled,
the default). Update at runtime without a restart:

```
PUT /config/slow_query_threshold_ms
Authorization: Bearer <superuser-token>
{ "threshold_ms": 100 }
```

Once enabled, every SQL statement whose wall-clock exceeds the threshold is:
1. Emitted as `tracing::warn` (target `unidb::slow_query`), carrying the
   `request_id`/`txn_id` correlation tags from item 22.
2. Appended to the 32-entry bounded ring surfaced by `GET /stats` →
   `recent_slow_queries[]`.

### Stats-history ring buffer (Part B)

The engine maintains a 300-point ring (≈ 25 min at the default 5 s tick).
The background ticker is started by `EngineHandle::spawn` (the server path),
so `Engine::open()` alone — used by all deterministic tests — never starts a
background thread. Tests can inject snapshots manually with
`engine.capture_stats_point()`.

```
GET /stats/history?points=60&interval_ms=5000
Authorization: Bearer <token>
```

Returns `{ interval_ms, points: [{t, commits, aborts, active_transactions,
wal_bytes, commits_per_sec, wal_bytes_per_sec, bufferpool_hit_ratio}] }`.
Rate fields are computed server-side from consecutive ring entries so the
Studio can replace its client-side delta math. Empty `points: []` on a fresh
engine (not an error). Points are oldest-first.

---

## 12. Logical replication and time-based PITR (item 28)

### Time-based PITR

Point-in-time restore to a wall-clock timestamp (not just a raw LSN) is
available via `Engine::restore_to_time`. See `docs/ops_runbook.md §9` for the
operator recipe, including the archive-wal + restore workflow.

Key facts for builders:
- `archive_wal(archive_dir)` now archives both WAL segments **and**
  `timeline.bin` (the (ts, lsn) mark file).
- `Engine::restore_to_time(base, archive, dest, target_ts_micros)` is a free
  function (no live engine required).
- Resolution: one mark per committed user transaction.
- Time is advisory; LSN is authoritative.

### Logical replication (unidb-logical crate)

The `unidb-logical` workspace crate allows applying a table-subset of
`INSERT`/`UPDATE`/`DELETE` changes from a primary engine to a target engine,
building on the item-26 event stream and item-20 dispatcher.

```rust
use std::sync::Arc;
use unidb::Engine;
use unidb_logical::{LogicalReplicator, TableSpec};

let primary = Arc::new(Engine::open("primary_dir", 0)?);
let target  = Arc::new(Engine::open("target_dir",  0)?);

// Enable events on the tables you want to replicate (primary side).
primary.enable_events("orders")?;

// Target schema must be pre-created — the replicator applies DML only.

let replicator = LogicalReplicator::builder(
    primary.clone(),
    target.clone(),
    "my-replica-consumer",   // unique per replication target
    vec![TableSpec {
        table:      "orders".to_string(),
        key_column: "id".to_string(),   // for UPDATE/DELETE identification
    }],
)
.build();

// In an async context: drive one poll-apply-ack cycle.
replicator.run_once().await?;

// Or drive continuously until shutdown.
let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
replicator.run(async { let _ = shutdown_rx.await; }).await;
```

**Delivery semantics:** at-least-once. The consumer offset is durably stored in
`__consumers__` on the primary. After a primary restart, resume with the same
`consumer_name` and the replicator picks up from the last acked event — no
committed change is lost.

**UPDATE events:** carry the new row image only (old key not present). The
logical apply reconstructs via `DELETE WHERE key = new_key + INSERT new_row`.
This is correct when the key column is immutable. If key-column-updates are
required, an item-26 follow-up (capturing `(old_key, new_row)` in UPDATE events)
will close the gap without a WAL format change.

**Tables not in scope:** events for tables not listed in `TableSpec` are silently
skipped — they are polled and acked normally but cause no write on the target.

