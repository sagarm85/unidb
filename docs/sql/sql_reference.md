# unidb SQL Reference

> The supported SQL surface, one command per section, each with syntax and a
> **runnable example**. Click a command in the index to jump straight to it.
>
> unidb speaks a **practical subset** of SQL (not full ANSI) plus a few
> engine-specific extensions — vector search (`NEAR`, `VECTOR(n)`),
> full-text (`MATCH`), a Cypher subset for graph, and Supabase-style
> row-level security. Every example on this page is executed against the engine
> by `examples/verify_sql_reference.rs` before release, so the syntax here is
> what the parser actually accepts. When this page and the code disagree, the
> code wins — please file it.

**Entry points (important):** most statements run through the embedded
`Engine::execute_sql(xid, sql)` or `POST /sql`. Two families use different
paths:

| Family | Entry point |
|---|---|
| DDL / DML / queries / vector / full-text | `execute_sql` · REST `POST /sql` · `POST /batch-sql` |
| **Auth &amp; RLS DDL** (`CREATE USER/ROLE`, `GRANT/REVOKE`, `CREATE POLICY`) | **`execute_sql_as(user, xid, sql)`** · REST auth surface — **not** plain `execute_sql` |
| **Graph reads** (`MATCH … RETURN`) | **`execute_cypher(xid, query)`** — a separate Cypher entry point |

---

## Command index

**Schema (DDL)** ·
[CREATE TABLE](#create-table) ·
[CREATE INDEX](#create-index) ·
[ALTER TABLE](#alter-table) ·
[DROP TABLE](#drop-table) ·
[TRUNCATE](#truncate) ·
[ANALYZE](#analyze)

**Data (DML)** ·
[INSERT](#insert) ·
[UPDATE](#update) ·
[DELETE](#delete) ·
[RETURNING](#returning)

**Queries** ·
[SELECT](#select) ·
[WHERE](#where) ·
[JOIN](#join) ·
[GROUP BY](#group-by) ·
[ORDER BY](#order-by) ·
[EXPLAIN](#explain)

**Search &amp; graph** ·
[NEAR (vector)](#near-vector-search) ·
[MATCH (full-text)](#match-full-text-search) ·
[MATCH … RETURN (Cypher / graph)](#cypher-match--return-graph)

**Security &amp; RLS** ·
[CREATE USER / ROLE](#create-user--role) ·
[GRANT / REVOKE](#grant--revoke) ·
[CREATE POLICY](#create-policy) ·
[current_user](#current_user)

**Transactions** ·
[BEGIN / COMMIT / ROLLBACK](#transactions)

---

## Compatibility at a glance

| Command | Status | Notes |
|---|---|---|
| `SELECT` (`WHERE`, `JOIN … USING`, `GROUP BY`, `ORDER BY`, aggregates) | ✅ Supported | `ORDER BY` accepts output columns, 1-based positions, and non-projected expressions (G4) |
| `INSERT` / `UPDATE` / `DELETE` (+ `RETURNING`) | ✅ Supported | multi-row `VALUES`; batched WAL |
| `CREATE TABLE` (`PRIMARY KEY`, `UNIQUE`, `REFERENCES`, `VECTOR(n)`) | ✅ Supported | practical column-constraint subset |
| `CREATE INDEX … USING BTREE \| HNSW \| FULLTEXT` | ✅ Supported | HNSW = vector; FULLTEXT = inverted index |
| `ALTER TABLE ADD/DROP COLUMN`, `DROP TABLE`, `TRUNCATE`, `ANALYZE`, `EXPLAIN` | ✅ Supported | |
| `NEAR(col, vec, k)` vector search · `MATCH(col, 'terms')` full-text | ✅ Supported | engine extensions |
| `MATCH (a)-[:TYPE]->(b) … RETURN` | ✅ Supported (read-only, Cypher subset) | via `execute_cypher`; edges written via the embedded `create_edge` API |
| `CREATE USER/ROLE`, `GRANT/REVOKE`, `CREATE POLICY` (RLS, incl. `WITH CHECK`) | ✅ Supported | via `execute_sql_as` / REST auth |
| `IS NULL` / `IS NOT NULL` | ✅ Supported | G10 shipped |
| `CAST(expr AS type)` | ✅ Supported | G2-cast shipped; handles INT/FLOAT/TEXT/BOOL, NULL propagation |
| `UNION` / `UNION ALL` / `INTERSECT` / `EXCEPT` (+ chained set-ops) | ✅ Supported | G3 shipped |
| `FROM (SELECT …) AS alias` derived tables | ✅ Supported | G6 shipped; RLS applies inside |
| `IN (subquery)` / `NOT IN (subquery)` / `EXISTS` / scalar subquery | ✅ Supported | P4.c shipped; RLS applies inside WHERE-clause subqueries |
| Window functions (`ROW_NUMBER`, `RANK`, `DENSE_RANK`, `LAG`, `LEAD`, `SUM`, `AVG`, `COUNT`, `MIN`, `MAX`) `OVER (PARTITION BY … ORDER BY …)` | ✅ Supported | G7 shipped; whole-partition frame only; cumulative frames are a follow-up |
| `LIKE` / `NOT LIKE` / `ILIKE` | ✅ Supported | delivered under item 30 |
| CTEs (`WITH`), recursive CTEs, frame-based window functions, `FULL OUTER JOIN`, `NATURAL JOIN` | ❌ Not yet | tracked in `19_sql_surface_gaps.md` |

---

# Schema (DDL)

## CREATE TABLE

Create a table. Columns take a type and optional constraints. Supported types
include `INT`, `BIGINT`, `SMALLINT`, `SERIAL`/`BIGSERIAL`, `TEXT`, `BOOL`,
and the vector type `VECTOR(n)`.

```sql
CREATE TABLE <name> (
  <col> <type> [PRIMARY KEY] [UNIQUE] [NOT NULL] [DEFAULT <v>]
              [REFERENCES <table>(<col>)]
  [, ...]
  [, FOREIGN KEY (<col>) REFERENCES <table>(<col>)]
);
```

**Example**
```sql
CREATE TABLE customers (id INT PRIMARY KEY, name TEXT UNIQUE, active BOOL);
CREATE TABLE orders (
  id INT PRIMARY KEY,
  customer_id INT REFERENCES customers(id),
  amount INT,
  status TEXT
);
CREATE TABLE docs (id INT, body TEXT, embedding VECTOR(4));
```

> A `PRIMARY KEY`/`UNIQUE` column gets an implicit enforcement B-tree (O(log n)
> uniqueness checks). `REFERENCES` enables row-level foreign-key enforcement
> (child insert/update checks the parent; parent delete/update RESTRICTs a
> referenced row).

## CREATE INDEX

Create a secondary index. The `USING` clause selects the index engine.

```sql
CREATE INDEX [<name>] ON <table> USING BTREE    (<col>);   -- range / equality
CREATE INDEX [<name>] ON <table> USING HNSW     (<col>);   -- vector similarity
CREATE INDEX [<name>] ON <table> USING FULLTEXT (<col>);   -- full-text (inverted)
```

**Example**
```sql
CREATE INDEX idx_status ON orders USING BTREE (status);
CREATE INDEX idx_body   ON docs   USING FULLTEXT (body);
CREATE INDEX idx_emb    ON docs   USING HNSW (embedding);
```

> `USING HNSW` builds the vector similarity index that powers [`NEAR`](#near-vector-search);
> `USING FULLTEXT` builds the inverted index that powers [`MATCH`](#match-full-text-search).
> Index build uses sort-then-bulk-load (one durable transaction).

## ALTER TABLE

Add or drop a column.

```sql
ALTER TABLE <table> ADD COLUMN <col> <type> [DEFAULT <v>];
ALTER TABLE <table> DROP COLUMN <col>;
```

**Example**
```sql
ALTER TABLE customers ADD COLUMN tier INT DEFAULT 1;
ALTER TABLE customers DROP COLUMN tier;
```

## DROP TABLE

```sql
DROP TABLE <table>;
```

## TRUNCATE

Remove all rows (fast O(pages) path — much faster than an unfiltered `DELETE`).

```sql
TRUNCATE <table>;
```

## ANALYZE

Refresh table statistics so the planner's index-vs-scan cost model is accurate.
Run it after a large load or churn.

```sql
ANALYZE <table>;
```

---

# Data (DML)

## INSERT

```sql
INSERT INTO <table> [(<col>, ...)] VALUES (<v>, ...) [, (<v>, ...) ...];
```

**Example**
```sql
INSERT INTO customers (id, name, active) VALUES (1, 'alice', true);
INSERT INTO customers (id, name, active) VALUES (2, 'bob', false);
INSERT INTO docs (id, body, embedding) VALUES (1, 'invoice overdue', [0.1, 0.2, 0.3, 0.4]);
```

> Multi-row `VALUES` is batched into one WAL bracket per heap page (fast bulk
> insert); `UNIQUE` constraints are still enforced per row. Vector literals use
> `[..]` bracket syntax and must match the column's `VECTOR(n)` arity.

## UPDATE

```sql
UPDATE <table> SET <col> = <expr> [, ...] [WHERE <predicate>];
```

**Example**
```sql
UPDATE orders SET status = 'shipped' WHERE id = 10;
```

> Updating a non-indexed column uses a HOT chain (no B-tree write). Under RLS,
> an UPDATE both filters the rows it may touch (`USING`) and validates the new
> row (`WITH CHECK`) — see [CREATE POLICY](#create-policy).

## DELETE

```sql
DELETE FROM <table> [WHERE <predicate>];
```

**Example**
```sql
DELETE FROM orders WHERE id = 10;
```

> An unfiltered `DELETE FROM <table>` with no FK children / CDC routes through
> the fast truncate path. To empty a table always prefer [`TRUNCATE`](#truncate).

## RETURNING

`INSERT`, `UPDATE`, and `DELETE` accept a trailing `RETURNING` list.

```sql
DELETE FROM <table> WHERE <predicate> RETURNING <col> [, ...];
```

**Example**
```sql
DELETE FROM orders WHERE id = 10 RETURNING id, status;
```

---

# Queries

## SELECT

```sql
SELECT <col> [, ...] | * | COUNT(*) | <agg>(<col>)
FROM <table>
[JOIN <table> USING (<col> [, ...])]
[WHERE <predicate>]
[GROUP BY <col> [, ...]]
[ORDER BY <col | position>]
[LIMIT <n>];
```

**Example**
```sql
SELECT id, name FROM customers WHERE active = true;
SELECT COUNT(*) FROM customers;
```

> Aggregates: `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`. `SELECT COUNT(*)` with no
> `WHERE` uses an O(1) catalog counter (not a scan) unless the table has an RLS
> policy, in which case it counts visible rows.

## WHERE

Predicates combine columns, literals, comparisons (`=`, `<>`, `<`, `<=`, `>`,
`>=`), and `AND`/`OR`. A predicate on an indexed column can use the B-tree; the
planner's size-aware cost model (after [`ANALYZE`](#analyze)) picks index vs
sequential scan.

**Example**
```sql
SELECT * FROM customers WHERE id = 1 AND active = true;
```

## JOIN

Equi-join on shared column name(s) with `USING`.

```sql
SELECT <cols> FROM <left> JOIN <right> USING (<col> [, ...]);
```

**Example**
```sql
SELECT * FROM orders JOIN customers USING (id);
```

> The planner picks a hash join, merge join, or index-nested-loop join based on
> statistics. (`ON <expr>` join syntax is not part of the v1 subset — use `USING`.)

## GROUP BY

```sql
SELECT <col>, <agg>(*) FROM <table> GROUP BY <col> [, ...];
```

**Example**
```sql
SELECT status, COUNT(*) FROM orders GROUP BY status;
```

> Grouped aggregation is pushed into parallel scan workers (partial aggregates).
> `GROUP BY ALL` and `HAVING` are not in the v1 subset.

## ORDER BY

Sort by an **output column name or a 1-based position** (arbitrary expressions
are not yet supported).

```sql
SELECT <cols> FROM <table> ORDER BY <output-col | position>;
```

**Example**
```sql
SELECT name FROM customers ORDER BY id;
SELECT name, id FROM customers ORDER BY 2;
```

## EXPLAIN

Show the chosen plan without running it.

```sql
EXPLAIN <select-statement>;
```

**Example**
```sql
EXPLAIN SELECT name FROM customers WHERE id = 1;
```

---

# Search &amp; graph

## NEAR (vector search)

Approximate nearest-neighbour search over a `VECTOR(n)` column, backed by an
HNSW index. Returns the `k` closest rows; the virtual `vec_distance` column is
available in the projection and orders results ascending.

```sql
SELECT <cols> FROM <table> WHERE NEAR(<vector_col>, [<v1>, <v2>, ...], <k>);
```

**Example**
```sql
SELECT * FROM docs WHERE NEAR(embedding, [0.0, 0.0, 0.0, 0.0], 3);
SELECT * FROM docs WHERE NEAR(embedding, [0.1, 0.2, 0.3, 0.4], 5) AND id > 0;
```

> The query vector's arity must match `VECTOR(n)`. Build the index first with
> [`CREATE INDEX … USING HNSW`](#create-index). Note gap **G10**: some row-path
> predicates (`IS NULL`, `LIKE`) combined with `NEAR` are not fully supported yet.

## MATCH (full-text search)

Full-text search over a `TEXT` column backed by a `FULLTEXT` index. Multiple
terms are matched against the inverted index.

```sql
SELECT <cols> FROM <table> WHERE MATCH(<text_col>, '<terms>');
```

**Example**
```sql
SELECT id FROM docs WHERE MATCH(body, 'invoice');
SELECT id FROM docs WHERE MATCH(body, 'invoice overdue');
```

## Cypher: MATCH … RETURN (graph)

Graph traversal uses a **Cypher subset** through the separate
`execute_cypher(xid, query)` entry point (not `execute_sql`). It is **read-only**:
edges are created with the embedded `create_edge(xid, from, to, type, props)` API
(or by writing the `__edges__` system table), then queried with `MATCH`.

```cypher
MATCH (a)-[:<TYPE>]->(b) [WHERE <predicate>] RETURN b [, type, props | b.<prop>]
```

**Example**
```cypher
MATCH (a)-[:KNOWS]->(b) WHERE a = 1 RETURN b
MATCH (a)-[:KNOWS]->(b) RETURN b, type, props
MATCH (a)-[]->(b) RETURN b
```

> A `WHERE a = <id>` predicate uses the adjacency index fast path; without it the
> traversal falls back to a full edge scan. Property access in `RETURN` is
> limited — see the graph engine doc for the exact subset.

---

# Security &amp; RLS

> These statements run through **`execute_sql_as(user, xid, sql)`** (embedded) or
> the REST auth surface — **not** plain `execute_sql`. Row-level security filters
> reads and validates writes per the acting user.

## CREATE USER / ROLE

```sql
CREATE USER <name> [SUPERUSER];
CREATE ROLE <name>;
```

**Example**
```sql
CREATE USER carol;
CREATE USER boss SUPERUSER;
CREATE ROLE analyst;
```

> Creating the first `USER` also exits "bootstrap mode": until at least one user
> exists, privileges and policies are not enforced.

## GRANT / REVOKE

Grant or revoke per-table operation privileges (or role membership).

```sql
GRANT  <SELECT | INSERT | UPDATE | DELETE | ALL> [, ...] ON <table> TO   <user|role>;
REVOKE <SELECT | INSERT | UPDATE | DELETE | ALL> [, ...] ON <table> FROM <user|role>;
GRANT  <role> TO <user>;
```

**Example**
```sql
GRANT SELECT, INSERT ON customers TO carol;
REVOKE INSERT ON customers FROM carol;
```

## CREATE POLICY

Row-level security policy. `USING` filters which rows an operation can see/touch;
`WITH CHECK` (defaults to `USING` when omitted) validates the **new** row on
`INSERT`/`UPDATE`. Policies are per-operation (`FOR SELECT | INSERT | UPDATE |
DELETE | ALL`).

```sql
CREATE POLICY <name> ON <table>
  FOR <SELECT | INSERT | UPDATE | DELETE | ALL>
  USING (<predicate>)
  [WITH CHECK (<predicate>)];
```

**Example**
```sql
CREATE POLICY sel_own ON docs   FOR SELECT USING (id > 0);
CREATE POLICY upd_chk ON orders FOR UPDATE USING (amount >= 0) WITH CHECK (amount >= 0);
CREATE POLICY own     ON todos  FOR SELECT USING (user_id = current_user);
```

> Both `USING` and `WITH CHECK` predicates must be parenthesised and non-empty.
> Index the policy's predicate column — the policy is merged into the plan as a
> `WHERE` clause, so an indexed column keeps it fast.

## current_user

The identity SQL function, resolved from the acting user (the JWT `sub` on the
REST path, or the `user` argument to `execute_sql_as`). Use it in policy
predicates for per-user data isolation (the Supabase `auth.uid()` analog).

**Example**
```sql
CREATE POLICY own ON todos FOR SELECT USING (user_id = current_user);
-- alice then sees only rows where user_id = 'alice'
SELECT * FROM todos;
```

---

# Transactions

`BEGIN`/`COMMIT`/`ROLLBACK` bracket a multi-statement transaction. In the
embedded API these correspond to `Engine::begin()`, `commit(xid)`, and
`abort(xid)`; over REST, transaction sessions expose the same semantics. Default
isolation is **Read Committed**; **Repeatable Read** and **Serializable (SSI)**
are available.

```sql
BEGIN;
  UPDATE accounts SET balance = balance - 100 WHERE id = 1;
  UPDATE accounts SET balance = balance + 100 WHERE id = 2;
COMMIT;   -- or ROLLBACK;
```

> A single transaction can touch all four data models (rows + vectors + graph
> edges + events) and commits atomically with **one** durable fsync — the
> engine's core differentiator.

---

_Verified against the engine by `examples/verify_sql_reference.rs`. Open gaps are
tracked in [`docs/backlog/19_sql_surface_gaps.md`](../backlog/19_sql_surface_gaps.md);
for the REST surface see [`docs/REST_API.md`](../REST_API.md)._
