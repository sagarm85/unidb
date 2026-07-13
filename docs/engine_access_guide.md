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
- **SELECT**: projections, `WHERE`, `INNER`/`LEFT`/`RIGHT`/`CROSS JOIN` with
  `ON <expr>` **or `USING (cols)`** (shared columns merged per standard SQL),
  `GROUP BY` / `HAVING`, aggregates (`COUNT`/`SUM`/`AVG`/`MIN`/
  `MAX`), `DISTINCT`, `ORDER BY`, `LIMIT`/`OFFSET`, subqueries (`IN`, `EXISTS`,
  scalar, correlated), and CTEs (`WITH`).
- **Transactions**: `READ COMMITTED` (default), `REPEATABLE READ`,
  `SERIALIZABLE` (SSI).
- **`EXPLAIN [ANALYZE]`**: renders the chosen plan (and, with `ANALYZE`, actual
  row counts + timing).
- **Domain extensions**: `VECTOR(n)` columns + `NEAR` similarity, full-text
  search, graph edges (Cypher subset via a separate entry point), a WAL-derived
  event queue.

**Not supported yet** — so you don't guess:

- `NATURAL JOIN` — use explicit `JOIN … USING (cols)` or `ON`.
- `FULL OUTER JOIN` (needed for a true `COALESCE`-merged `USING` column on both
  outer sides; `INNER`/`LEFT`/`RIGHT` `USING` are supported).
- `ORDER BY` on an expression that is **not** in the SELECT output list (order by
  a projected column name/alias or ordinal position).
- Set operations (`UNION`/`INTERSECT`/`EXCEPT`), window functions, `RETURNING`.
- Foreign-key enforcement at the **row** level: `FOREIGN KEY` **is parsed,
  persisted, and introspectable** (see [§4](#4-introspect-the-system-catalog)),
  but M11 enforces only that the *referenced table exists* — referenced-**row**
  existence and `ON DELETE`/`ON UPDATE` actions are a filed follow-up.
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
JSON object. These field names are the contract:

| Field | Type | Meaning |
|---|---|---|
| `seq` | integer | **Monotonic per-database offset**, assigned in commit order. This *is* the cursor/"LSN" of the stream: consumers resume from, ack, and dedupe on `seq`. Dense and gap-free across enabled tables. |
| `xid` | integer | Transaction id that produced the event. All events sharing an `xid` were committed atomically together (use it to reassemble a multi-row/multi-table transaction). |
| `table_name` | string | Source table. |
| `op` | string | `"insert"`, `"update"`, or `"delete"`. |
| `payload` | object | **Full row image** keyed by column name — the post-image for insert/update, the pre-image for delete. Typed exactly as [§5](#5-results-types--paging) describes (decimals/timestamps as canonical strings, JSON columns embedded not double-encoded, vectors as arrays). |

> **Honest scope of the contract.** `seq` is the ordering key and offset — unidb
> does **not** expose the physical WAL byte-LSN as a separate event field (it
> would leak a storage-internal number with no consumer use that `seq` doesn't
> already serve), and there is **no wall-clock `timestamp` column** on the event
> today: the stream carries commit *order*, not commit *time*. A consumer that
> needs receipt time stamps it on arrival; a producer that needs event time puts
> it in its own row column, where it rides through in `payload`. The engine emits
> raw row-level facts and transforms nothing — all shaping is consumer-side.

### 8.2 Consuming: durable consumers vs. live tail

Two models, one stream:

- **Durable consumer (at-least-once).** Poll with a named consumer; the engine
  tracks that consumer's **durable** offset. Process the batch, then `ack` up to
  the last `seq` you handled. A crash before ack ⇒ redelivery (dedupe on `seq`).
  This is the Kafka manual-commit shape.
  - Embed: `poll_events(xid, "billing", limit)` → process → `ack_events(xid, "billing", up_to_seq)`.
  - Server: `GET /events/subscribe?consumer=billing` (SSE) → `POST /events/ack`.
    Acks travel over `/events/ack`, never over the SSE connection.
- **Ephemeral live tail (at-most-once).** For a browser `EventSource` that just
  wants to watch a table: `GET /events/subscribe` with **no** `consumer`. Nothing
  is written to the consumer registry. Resume-from-offset on reconnect is the
  standard SSE `Last-Event-ID` header (each frame carries `id: <seq>`), or an
  explicit `?from_seq=<seq>` for offset scrubbing / replay-from-offset. An
  optional `?table=<name>` filters to one table. Heartbeats keep the idle
  connection open.

### 8.3 Replay & the vacuum-horizon contract

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

### 8.4 The reference dispatcher (`unidb-dispatch`)

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

---

## 9. Honest limitations

- **FK is metadata-only (M11).** Foreign keys parse, persist, and introspect, but
  only referenced-*table* existence is enforced — not referenced-*row* existence,
  and there are no `ON DELETE`/`ON UPDATE` actions yet. The catalog reports
  `update_rule`/`delete_rule` as `NO ACTION` and `match_option` as `NONE`
  accordingly.
- **Constraint names are synthesized, not stored.** `<table>_pkey` etc. — stable
  and deterministic, but not names you declared (unidb has no named-constraint
  syntax).
- **Reconstructed DDL is canonical, not byte-identical** (see [§4](#object-ddl--reconstruct-from-metadata-c5)).
- **`is_unique` on `unidb_catalog.indexes`** reflects whether the indexed column
  carries a UNIQUE/PK *constraint*, not a property of the index itself — unidb
  secondary indexes are non-unique structures; uniqueness is enforced by the
  constraint, not the index.
- **One schema, one database per server.** `'public'` / `'unidb'` are constants;
  there is no schema namespacing and no multi-db addressing.
- **No `NATURAL JOIN` / `FULL OUTER JOIN`** (`JOIN … USING` *is* supported —
  `INNER`/`LEFT`/`RIGHT`; see [§2](#2-query-the-sql-surface)).

---

*Milestone 18 (engine access & introspection contract). See
`docs/backlog/18_engine_access_contract.md` for the spec + design note, and
`docs/REST_API.md` for the exhaustive HTTP route/error reference. §8 (event
stream / change events) was added by Milestone 20 —
`docs/backlog/20_events_realtime_dispatcher.md`.*
